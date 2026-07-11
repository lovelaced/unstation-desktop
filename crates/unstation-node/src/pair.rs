//! `unstation-node pair` — terminal sign-in with the Polkadot app.
//!
//! Presentation only: renders the pairing QR + progress in the terminal and
//! persists the resulting slot secret via [`crate::keydir`]. The chain/SDK work
//! lives behind [`PairBackend`] so this module tests without a phone or a
//! network (see the mock in the tests below).

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use unstation_chain::sso_pair::{PairEvent, PairOutcome};

use crate::keydir;

/// Overall pairing budget: QR scan + phone approval + on-chain allowance can
/// take a few minutes of human time.
const PAIR_TIMEOUT: Duration = Duration::from_secs(420);

/// The chain-facing side of `pair`, mocked in tests.
pub trait PairBackend {
    fn pair(
        &mut self,
        key_dir: &Path,
        product_id: &str,
        timeout: Duration,
        on_event: &mut dyn FnMut(PairEvent),
    ) -> Result<PairOutcome, String>;
}

/// The real backend: `unstation_chain::sso_pair::pair_with_phone`.
pub struct ChainPairBackend;

impl PairBackend for ChainPairBackend {
    fn pair(
        &mut self,
        key_dir: &Path,
        product_id: &str,
        timeout: Duration,
        on_event: &mut dyn FnMut(PairEvent),
    ) -> Result<PairOutcome, String> {
        unstation_chain::sso_pair::pair_with_phone(key_dir, product_id, timeout, on_event)
    }
}

/// Entry point for the `pair` subcommand. Returns the process exit code:
/// 0 = paired (or already paired), 1 = pairing failed, 2 = usage.
pub fn run(
    backend: &mut dyn PairBackend,
    key_dir: &Path,
    args: &[String],
    out: &mut dyn Write,
) -> i32 {
    let mut force = false;
    for a in args {
        match a.as_str() {
            "--force" => force = true,
            other => {
                let _ = writeln!(out, "unknown option for pair: {other}  (usage: unstation-node pair [--force])");
                return 2;
            }
        }
    }

    match keydir::read_slot_secret(key_dir) {
        Ok(Some(_)) if !force => {
            let _ = writeln!(
                out,
                "already signed in (slot key in {}) — use `pair --force` to re-pair",
                key_dir.display()
            );
            return 0;
        }
        Ok(_) => {}
        Err(e) if force => {
            // --force exists to recover from exactly this; note it and re-pair.
            let _ = writeln!(out, "replacing unreadable slot key: {e}");
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            return 1;
        }
    }

    let mut on_event = |ev: PairEvent| match ev {
        PairEvent::QrReady { uri } => {
            let _ = writeln!(out, "\nScan this with the Polkadot app on your phone:\n");
            match render_qr_unicode(&uri) {
                Ok(qr) => {
                    let _ = writeln!(out, "{qr}");
                }
                Err(e) => {
                    let _ = writeln!(out, "(couldn't draw the QR here: {e})");
                }
            }
            let _ = writeln!(out, "Or open this link on the phone:\n  {uri}\n");
            let _ = writeln!(out, "Waiting for the phone (the code expires after ~2 minutes)…");
        }
        PairEvent::Paired { address, display_name } => {
            let _ = writeln!(out, "✓ paired with {display_name} ({address})");
            let _ = writeln!(out, "  requesting the storage allowance — approve it on the phone…");
        }
        PairEvent::Info { msg } => {
            let _ = writeln!(out, "  {msg}");
        }
    };

    match backend.pair(key_dir, keydir::SEED_PRODUCT_ID, PAIR_TIMEOUT, &mut on_event) {
        Ok(outcome) => {
            if let Err(e) = keydir::write_slot_secret(key_dir, &outcome.slot_secret) {
                let _ = writeln!(out, "error: pairing succeeded but saving the key failed: {e}");
                return 1;
            }
            let _ = writeln!(out, "✓ allowance granted");
            let _ = writeln!(
                out,
                "✓ identity saved to {} (account {})",
                key_dir.display(),
                hex32(&outcome.identity_public)
            );
            let _ = writeln!(
                out,
                "\nDone. Start the seed (or under systemd: sudo systemctl restart unstation-seed)."
            );
            0
        }
        Err(e) => {
            let _ = writeln!(out, "\npairing failed: {e}");
            let _ = writeln!(out, "nothing was saved — re-run `unstation-node pair` to try again");
            1
        }
    }
}

/// Half-block Unicode QR (two modules per character row) — compact enough for a
/// default 80×24 SSH terminal at this URI length.
fn render_qr_unicode(uri: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    struct MockBackend {
        result: Option<Result<PairOutcome, String>>,
        saw_product_id: Option<String>,
    }

    impl MockBackend {
        fn ok(secret: Vec<u8>) -> Self {
            Self {
                result: Some(Ok(PairOutcome {
                    slot_secret: Zeroizing::new(secret),
                    identity_public: [0xAB; 32],
                    phone_address: "15oF4…".into(),
                    display_name: "Erin's phone".into(),
                })),
                saw_product_id: None,
            }
        }
        fn failing(msg: &str) -> Self {
            Self { result: Some(Err(msg.into())), saw_product_id: None }
        }
    }

    impl PairBackend for MockBackend {
        fn pair(
            &mut self,
            _key_dir: &Path,
            product_id: &str,
            _timeout: Duration,
            on_event: &mut dyn FnMut(PairEvent),
        ) -> Result<PairOutcome, String> {
            self.saw_product_id = Some(product_id.to_string());
            on_event(PairEvent::QrReady { uri: "polkadotapp://pair?handshake=00ff".into() });
            on_event(PairEvent::Paired {
                address: "15oF4…".into(),
                display_name: "Erin's phone".into(),
            });
            self.result.take().expect("pair called once")
        }
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("unstation-pair-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn happy_path_prints_qr_and_persists_the_secret() {
        let dir = tmp_dir("happy");
        let mut backend = MockBackend::ok(vec![5u8; 64]);
        let mut out = Vec::new();
        let code = run(&mut backend, &dir, &[], &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "output:\n{text}");
        assert!(text.contains("polkadotapp://pair?handshake=00ff"), "raw URI fallback printed");
        assert!(text.contains("▄") || text.contains("█"), "a rendered QR block appears");
        assert!(text.contains("allowance granted"));
        assert_eq!(backend.saw_product_id.as_deref(), Some(keydir::SEED_PRODUCT_ID));
        let saved = keydir::read_slot_secret(&dir).unwrap().expect("slot saved");
        assert_eq!(&saved[..], &[5u8; 64]);
    }

    #[test]
    fn already_paired_is_a_noop_without_force() {
        let dir = tmp_dir("noop");
        keydir::write_slot_secret(&dir, &[1u8; 64]).unwrap();
        let mut backend = MockBackend::failing("must not be called");
        let mut out = Vec::new();
        let code = run(&mut backend, &dir, &[], &mut out);
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).unwrap().contains("already signed in"));
        assert!(backend.saw_product_id.is_none(), "backend must not run when already paired");
        assert_eq!(&keydir::read_slot_secret(&dir).unwrap().unwrap()[..], &[1u8; 64]);
    }

    #[test]
    fn force_re_pairs_over_an_existing_secret() {
        let dir = tmp_dir("force");
        keydir::write_slot_secret(&dir, &[1u8; 64]).unwrap();
        let mut backend = MockBackend::ok(vec![2u8; 64]);
        let mut out = Vec::new();
        let code = run(&mut backend, &dir, &["--force".into()], &mut out);
        assert_eq!(code, 0);
        assert_eq!(&keydir::read_slot_secret(&dir).unwrap().unwrap()[..], &[2u8; 64]);
    }

    #[test]
    fn failure_saves_nothing_and_exits_nonzero() {
        let dir = tmp_dir("fail");
        let mut backend = MockBackend::failing("phone said no");
        let mut out = Vec::new();
        let code = run(&mut backend, &dir, &[], &mut out);
        assert_eq!(code, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("phone said no"));
        assert!(keydir::read_slot_secret(&dir).unwrap().is_none(), "no partial state on failure");
    }

    #[test]
    fn unknown_flag_is_usage_error() {
        let dir = tmp_dir("usage");
        let mut backend = MockBackend::failing("unused");
        let mut out = Vec::new();
        assert_eq!(run(&mut backend, &dir, &["--wat".into()], &mut out), 2);
    }
}
