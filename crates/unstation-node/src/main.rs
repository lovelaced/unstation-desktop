//! `unstation-node` — a headless volunteer seed/relay (TECH_SPEC §8.5 / D7).
//!
//! The decentralized answer to a TURN/CDN tier: anyone can run this binary anywhere
//! and it joins stream swarms as `Role::Seed` nodes — fetching the live window
//! like a viewer (every segment hash-verified against the publisher's signed live
//! edge), caching it, and reserving its uplink for peers who can't reach the
//! publisher directly. More volunteers = more capacity and more reachable entry
//! points, with no operator to take down. It plays nothing and stores nothing
//! beyond the rolling live window.
//!
//! Usage:
//!   unstation-node [--stream <name>]... [--open]   run the seed
//!   unstation-node pair [--force]      sign in with the Polkadot app (QR in the
//!                                      terminal); persists an allowance-backed
//!                                      slot key in the key dir
//!
//! Bare `unstation-node` (no args) runs as an OPEN relay: no stream is configured
//! up front. It announces spare capacity on the global volunteer rendezvous and
//! serves whatever streams publishers recruit it onto — each recruitment's manifest
//! is verified against the recruiting publisher before a byte is fetched, and a
//! policy layer handles admission, budget splits, idle/stall eviction, and parking
//! (dormant chain polling) while nobody watches. `--stream <name>` (repeatable)
//! pins named streams instead — pins never evict; add `--open` to also volunteer
//! the remaining capacity.
//!
//! Identity (statement-store writes need an on-chain allowance), in precedence order:
//!   <key-dir>/slot_secret      phone-paired slot key written by `pair` (public chain)
//!   UNSTATION_NODE_MNEMONIC    sign with a pre-provisioned account instead
//!   <key-dir>/peer_key         generated key — dev chains only, where the e2e
//!                              harness provisions it out-of-band; on the public
//!                              chain boot fails loud (exit 78) with pair instructions
//!   UNSTATION_NODE_KEY_DIR     the key dir (default ~/.unstation-node)
//!
//! Tuning:
//!   UNSTATION_NODE_BUDGET_MBPS  total uplink to donate across streams (default 50)
//!   UNSTATION_NODE_MAX_STREAMS  most streams served at once (default 8)
//!   UNSTATION_STUN / UNSTATION_TURN  ICE servers (comma-separated)
//!   HOST_STATEMENT_STORE_WS_ENDPOINTS  override the statement-store endpoints

use std::time::Duration;

mod keydir;
mod pair;
mod policy;
mod recruit;
mod streams;
mod supervisor;
mod worker;

use supervisor::Supervisor;

/// Exit code for "identity unusable — pairing/config required" (EX_CONFIG). The systemd
/// unit lists this in `RestartPreventExitStatus`: restarting cannot fix it, a human must
/// run `unstation-node pair` (or set UNSTATION_NODE_MNEMONIC). Transient failures exit 1
/// and are restarted as usual.
const EXIT_CONFIG: i32 = 78;

const USAGE: &str = "usage: unstation-node [--stream <name>]... [--open]  |  unstation-node pair [--force]   \
                     (bare = open relay; see the header of main.rs for env config)";

/// ICE servers — same env contract as the app.
fn stun() -> Vec<String> {
    let mut servers: Vec<String> = match std::env::var("UNSTATION_STUN") {
        Ok(v) => v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect(),
        Err(_) => vec![
            "stun:stun.l.google.com:19302".into(),
            "stun:stun.cloudflare.com:3478".into(),
        ],
    };
    if let Ok(turn) = std::env::var("UNSTATION_TURN") {
        servers.extend(turn.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from));
    }
    servers
}

/// Parsed run mode. `pair` is dispatched in `main` BEFORE this parser — it never
/// sees the subcommand on a normal invocation.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    /// Stream names to pin (never evicted). Empty for a pure open relay.
    pins: Vec<String>,
    /// Volunteer on the rendezvous + accept recruitments.
    open: bool,
}

/// Pure CLI parse over everything after the program name. Bare → open relay;
/// `--stream <name>` (repeatable) pins; `--open` volunteers alongside any pins; a
/// bare positional is the deprecated single-stream form. `Err` carries the message
/// for stderr (main appends the usage line and exits 2).
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut pins = Vec::new();
    let mut open_flag = false;
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--stream" => match it.next() {
                Some(name) if !name.starts_with('-') => pins.push(name.clone()),
                _ => return Err("--stream needs a stream name".into()),
            },
            "--open" => open_flag = true,
            "--help" | "-h" => return Err(String::new()),
            "pair" => {
                // Dispatched before parsing on a real invocation; reaching here means
                // it wasn't first (`unstation-node --open pair` etc.) — refuse rather
                // than seed a stream literally named "pair".
                return Err("`pair` must be the first argument".into());
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag: {flag}")),
            positional => {
                // Deprecated back-compat: `unstation-node <stream-name>`.
                log::warn!(
                    "[seed] positional stream names are deprecated — use `--stream {positional}`"
                );
                pins.push(positional.to_string());
            }
        }
    }
    // Bare invocation = open relay; naming streams narrows to just them unless
    // --open re-broadens.
    let open = open_flag || pins.is_empty();
    Ok(Args { pins, open })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    // The pair flow is an interactive QR screen — keep the SDK's INFO chatter out of
    // it (RUST_LOG still overrides for debugging). The seed itself logs at info.
    let default_log = if args.get(1).map(String::as_str) == Some("pair") { "warn" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_log)).init();

    if args.get(1).map(String::as_str) == Some("pair") {
        // Sign-in subcommand: pair with the Polkadot app, persist the slot key, exit.
        // (A stream literally named "pair" is shadowed — pick another name.)
        let key_dir = keydir::default_key_dir();
        let code =
            pair::run(&mut pair::ChainPairBackend, &key_dir, &args[2..], &mut std::io::stdout());
        std::process::exit(code);
    }
    let parsed = match parse_args(&args[1..]) {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("error: {msg}");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    // ---- identity: an allowance-backed statement-store key (see keydir.rs) ----
    let key_dir = keydir::default_key_dir();
    let key_dir_str = key_dir.display().to_string();
    let mnemonic_env = std::env::var("UNSTATION_NODE_MNEMONIC").ok();
    match keydir::resolve_identity(&key_dir, mnemonic_env.as_deref()) {
        Ok(keydir::IdentitySource::PairedSlot(secret)) => {
            if let Err(e) = unstation_chain::init_statement_store_from_secret(&secret) {
                eprintln!(
                    "[seed] ERROR: corrupt slot key in {key_dir_str}: {e} — re-run `unstation-node pair --force`"
                );
                std::process::exit(EXIT_CONFIG);
            }
            log::info!(
                "[seed] identity: phone-paired slot key (product {})",
                keydir::SEED_PRODUCT_ID
            );
        }
        Ok(keydir::IdentitySource::Mnemonic(m)) => {
            if let Err(e) = unstation_chain::init_from_mnemonic(&m) {
                eprintln!("[seed] ERROR: identity from UNSTATION_NODE_MNEMONIC failed: {e}");
                std::process::exit(EXIT_CONFIG);
            }
            log::info!("[seed] identity: mnemonic-derived (pre-provisioned)");
        }
        Ok(keydir::IdentitySource::GeneratedLegacy) => {
            let _ = std::fs::create_dir_all(&key_dir);
            unstation_chain::init_statement_store_persisted(&key_dir);
            log::info!(
                "[seed] identity: generated key in {key_dir_str} — dev chains only; the public \
                 chain requires `unstation-node pair`"
            );
        }
        Err(e) => {
            eprintln!("[seed] ERROR: {e}");
            std::process::exit(EXIT_CONFIG);
        }
    }
    if !unstation_chain::wait_ready(Duration::from_secs(30)) {
        log::warn!("[seed] statement store not confirmed subscribed after 30s — continuing (it may still connect)");
    }

    // Writability gate: a key without an on-chain allowance connects fine and then has
    // every presence/signaling write silently rejected — the node would look alive while
    // being undiscoverable forever. Probe once at boot and fail loud instead.
    let probe = tokio::task::spawn_blocking(|| {
        unstation_chain::probe_submit_ready(Duration::from_secs(60))
    })
    .await
    .unwrap_or_else(|e| unstation_chain::SubmitReadiness::Unreachable(format!("probe task: {e}")));
    match probe {
        unstation_chain::SubmitReadiness::Ready => {}
        unstation_chain::SubmitReadiness::NoAllowance(e) => {
            eprintln!("[seed] ERROR: this seed's key has no statement-store allowance on the chain — it");
            eprintln!("[seed] cannot announce itself, so viewers will never find it. ({e})");
            eprintln!("[seed] Sign it in with your Polkadot app:");
            eprintln!("[seed]     UNSTATION_NODE_KEY_DIR={key_dir_str} unstation-node pair");
            eprintln!("[seed] (under systemd: sudo -u unstation UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed \\");
            eprintln!("[seed]  unstation-node pair && sudo systemctl restart unstation-seed)");
            eprintln!("[seed] Alternatively set UNSTATION_NODE_MNEMONIC to a pre-provisioned account.");
            std::process::exit(EXIT_CONFIG);
        }
        unstation_chain::SubmitReadiness::Unreachable(e) => {
            eprintln!("[seed] statement-store chain unreachable: {e} — exiting so the service manager can retry");
            std::process::exit(1);
        }
    }

    let cfg = policy::PolicyCfg::from_env();
    if parsed.open {
        log::info!(
            "[seed] open relay: volunteering {} stream slot(s) at {}Mbps total",
            cfg.max_streams,
            cfg.total_budget_bps / 1_000_000
        );
    }
    Supervisor::new(cfg, parsed.pins, parsed.open, stun()).run().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_invocation_is_an_open_relay() {
        assert_eq!(parse_args(&[]).unwrap(), Args { pins: vec![], open: true });
    }

    #[test]
    fn stream_flag_pins_and_closes() {
        assert_eq!(
            parse_args(&argv(&["--stream", "jet-live"])).unwrap(),
            Args { pins: vec!["jet-live".into()], open: false }
        );
    }

    #[test]
    fn stream_flag_repeats() {
        assert_eq!(
            parse_args(&argv(&["--stream", "a", "--stream", "b"])).unwrap(),
            Args { pins: vec!["a".into(), "b".into()], open: false }
        );
    }

    #[test]
    fn open_flag_rebroadens_pinned_runs() {
        assert_eq!(
            parse_args(&argv(&["--stream", "a", "--open"])).unwrap(),
            Args { pins: vec!["a".into()], open: true }
        );
        // Order doesn't matter.
        assert_eq!(
            parse_args(&argv(&["--open", "--stream", "a"])).unwrap(),
            Args { pins: vec!["a".into()], open: true }
        );
        // --open alone is just the bare open relay, spelled out.
        assert_eq!(parse_args(&argv(&["--open"])).unwrap(), Args { pins: vec![], open: true });
    }

    #[test]
    fn positional_back_compat_is_a_single_closed_pin() {
        assert_eq!(
            parse_args(&argv(&["seed-e2e"])).unwrap(),
            Args { pins: vec!["seed-e2e".into()], open: false }
        );
    }

    #[test]
    fn stream_flag_requires_a_name() {
        assert!(parse_args(&argv(&["--stream"])).is_err());
        assert!(parse_args(&argv(&["--stream", "--open"])).is_err());
    }

    #[test]
    fn pair_is_not_consumed_as_a_stream_name() {
        // main dispatches `pair` before parsing; if it reaches the parser it was
        // misplaced — never seed a stream literally named "pair".
        assert!(parse_args(&argv(&["pair"])).is_err());
        assert!(parse_args(&argv(&["--open", "pair"])).is_err());
    }

    #[test]
    fn unknown_flags_and_help_error_out() {
        assert!(parse_args(&argv(&["--bogus"])).is_err());
        assert_eq!(parse_args(&argv(&["--help"])), Err(String::new()));
        assert_eq!(parse_args(&argv(&["-h"])), Err(String::new()));
    }
}
