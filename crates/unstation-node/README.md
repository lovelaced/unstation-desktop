# unstation-node — run a volunteer seed

A headless seed/relay for one Unstation stream: the decentralized answer to a
TURN/CDN tier. It joins the stream's swarm like a viewer — every segment
hash-verified against the publisher's signed live edge — caches the rolling live
window, and donates its uplink to viewers who can't reach the publisher directly
(phones on carrier CGNAT, viewers behind symmetric NATs). It plays nothing,
stores nothing beyond the live window, and can't forge a byte: viewers verify
everything they receive from it.

More volunteers ⇒ more capacity and more reachable entry points, with no
operator to take down.

## Quickstart (Linux VPS)

```sh
curl -fsSL https://raw.githubusercontent.com/lovelaced/unstation-desktop/master/scripts/seed/install.sh \
  | sudo bash -s -- --stream <stream-name>
```

That downloads the latest release binary for your architecture (checksum-verified),
creates an `unstation` system user, and starts a hardened `unstation-seed`
systemd service. Watch it come up:

```sh
journalctl -fu unstation-seed
# expect: identity: persisted key … → joining swarm "<stream>" → [seg] seq=N verified …
```

The seed's on-chain identity is auto-provisioned on the public testnet on first
run and persisted in `/var/lib/unstation-seed` — keep that directory to keep the
identity.

## What you need

| Resource | Enough | Notes |
|---|---|---|
| CPU / RAM | 1–2 vCPU, 1 GB | hash-verify + DTLS on a ~6 Mbps stream is light |
| Disk | any | nothing persistent beyond a small key dir |
| Network | the real requirement | ~1× stream bitrate in, ~1× per served viewer out (≈2.7 GB/h per viewer at 6 Mbps) — prefer flat-traffic providers (Hetzner/Netcup/OVH) over metered-egress clouds |
| Reachability | public IP, inbound **UDP** allowed | WebRTC uses ephemeral UDP ports; no TCP ports, domains, or TLS certs |

## Configuration

The service file (`/etc/systemd/system/unstation-seed.service`) is the config —
edit, then `systemctl daemon-reload && systemctl restart unstation-seed`.

| Env / arg | Default | Meaning |
|---|---|---|
| argv[1] | — | stream name to seed (same name viewers type) |
| `UNSTATION_NODE_BUDGET_MBPS` | `50` | uplink to donate, aggregate |
| `UNSTATION_NODE_KEY_DIR` | `~/.unstation-node` | persisted identity (auto-provisioned on testnet builds) |
| `UNSTATION_NODE_MNEMONIC` | — | sign with a pre-provisioned account instead (overrides the key dir) |
| `UNSTATION_STUN` / `UNSTATION_TURN` | Google+Cloudflare STUN | ICE servers, comma-separated |
| `HOST_STATEMENT_STORE_WS_ENDPOINTS` | SDK defaults | override the statement-store chain endpoints |
| `RUST_LOG` | `info` | log level |

## Building from source

Binaries are produced by the `seed-release` workflow (push a `seed-v*` tag).
To build yourself you need this repo and the chain SDK as sibling checkouts
(`useragent-kit` next to `unstation-desktop`), then:

```sh
cd unstation-desktop/crates/unstation-node
cargo build --release
# target/release/unstation-node (also findable under the crate's own target dir)
```

Install it with the same script, skipping the download:

```sh
sudo scripts/seed/install.sh --stream <name> --binary path/to/unstation-node
```

## Operating notes

- **One stream per service.** To seed several streams, copy the unit under new
  names (`unstation-seed@foo.service` style is fine) — each instance is
  independent.
- **Trust:** the seed verifies the publisher's signed manifest before joining a
  candidate's swarm, and every segment hash-verifies against the signed live
  edge — a seed can withhold bandwidth, but never alter content.
- **Uninstall:** `systemctl disable --now unstation-seed && rm /etc/systemd/system/unstation-seed.service /usr/local/bin/unstation-node && rm -rf /var/lib/unstation-seed`.
