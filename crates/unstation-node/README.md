# unstation-node — run a volunteer relay

A headless volunteer relay: the decentralized answer to a TURN/CDN tier. It
joins stream swarms like a viewer — every segment hash-verified against the
publisher's signed live edge — caches the rolling live window, and donates its
uplink to viewers who can't reach the publisher directly (phones on carrier
CGNAT, viewers behind symmetric NATs). It plays nothing, stores nothing beyond
the live window, and can't forge a byte: viewers verify everything they receive
from it. Invite-only streams stay encrypted end to end; a relay carries them
without ever being able to watch.

By default it runs as an **open relay**: you don't pick a stream. It announces
spare capacity on the network and helps carry whatever streams recruit it, up
to a stream cap and a total upload budget, dropping streams nobody is watching.
`--stream <name>` pins it to specific streams instead.

More volunteers ⇒ more capacity and more reachable entry points, with no
operator to take down.

## Quickstart (Linux VPS)

```sh
curl -fsSL https://raw.githubusercontent.com/lovelaced/unstation-desktop/master/scripts/seed/install.sh \
  | sudo bash
```

That installs an open relay. To help specific streams only, add
`-s -- --stream <name>` (repeatable).

The installer downloads the latest release binary for your architecture
(checksum-verified), creates an `unstation` system user, then shows a QR code:
scan it with the Polkadot app on your phone to sign the seed in. The phone
grants the seed's key an on-chain statement-store allowance, which is what lets
it announce itself so viewers can find it. After that it starts a hardened
`unstation-seed` systemd service. Watch it come up:

```sh
journalctl -fu unstation-seed
# open relay: 'open relay: volunteering N stream slot(s)' now, 'joining swarm for' as streams recruit it
# pinned:     identity: phone-paired slot key … → joining swarm for "<stream>" → [seg] seq=N verified …
```

The seed's identity is persisted in `/var/lib/unstation-seed` — keep that
directory to keep the identity (you won't need to scan again).

## Signing in

The seed's chain writes (announcing itself, answering connection requests) need
an on-chain allowance, granted by the Polkadot app the same way it signs in the
desktop and phone apps. The installer handles this; to do it manually:

```sh
sudo -u unstation UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed unstation-node pair
sudo systemctl restart unstation-seed
```

`pair` prints a QR code in the terminal (SSH is fine) plus the raw
`polkadotapp://` link as a fallback; scan or open it with the Polkadot app and
approve the request. It's idempotent: if the seed is already signed in it says
so and exits. Use `pair --force` to re-pair (for example after replacing your
phone).

Non-interactive installs: pass `--no-pair` to the installer and pair later, or
set `UNSTATION_NODE_MNEMONIC` in the installer's environment to use a
pre-provisioned account instead (it's stored in a root-only env file, never in
process argv).

### Limitations (v1)

**One seed per operator phone.** The phone derives the seed's slot account
deterministically from the product id, so two seeds paired with the same phone
would share one on-chain identity and evict each other's announcements. To run
more seeds, use a different phone per seed or `UNSTATION_NODE_MNEMONIC` with
separately provisioned accounts.

**Shielded publishers only answer relays they recruited.** A publisher hiding
its connection admits only the volunteers it recruited itself, so a seed pinned
to its stream with `--stream` must also run `--open` to be recruitable by it.

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
| (no args) | open relay | volunteer for whatever streams recruit this seed |
| `--stream <name>` | — | pin a stream to seed, repeatable (same name viewers type); pins never evict. Add `--open` to also volunteer the leftover capacity |
| `UNSTATION_NODE_BUDGET_MBPS` | `50` | total uplink to donate, shared across streams |
| `UNSTATION_NODE_MAX_STREAMS` | `8` | open relay: most streams served at once (each needs at least ~4 Mbps of the budget) |
| `UNSTATION_NODE_KEY_DIR` | `~/.unstation-node` | persisted identity: the phone-paired slot key from `unstation-node pair` (public chain), or a generated key (dev chains) |
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
sudo scripts/seed/install.sh --binary path/to/unstation-node
```

## Operating notes

- **One service serves many streams.** The open relay joins and leaves swarms on
  demand (streams nobody watches are parked, then dropped), splitting the budget
  across whatever it's carrying. Pins (`--stream`) are never dropped.
- **Trust:** the seed verifies the publisher's signed manifest before joining a
  swarm — recruited streams are verified before a byte is fetched — and every
  segment hash-verifies against the signed live edge. A seed can withhold
  bandwidth, but never alter content.
- **Uninstall:** `systemctl disable --now unstation-seed && rm /etc/systemd/system/unstation-seed.service /usr/local/bin/unstation-node && rm -rf /var/lib/unstation-seed`.

## Troubleshooting

- **Service exits with status 78 / "no statement-store allowance"** — the seed
  isn't signed in on this chain. Run the `pair` command from *Signing in* above,
  then restart the service. (systemd deliberately doesn't restart on 78: a
  restart can't fix a missing sign-in.)
- **`chain_write_fail` climbing in the heartbeat line** — the seed's allowance
  stopped working after boot (for example the grant was revoked on the phone).
  Re-pair with `pair --force`, then restart.
- **Pairing times out** — the phone and the seed talk over the chain, not the
  local network, so distance doesn't matter, but both need connectivity. Check
  the phone is online, then re-run `pair`.
- **Moved to a new VPS?** Copy `/var/lib/unstation-seed` across (owned by the
  `unstation` user, mode 0600 files) and the identity moves with it; otherwise
  just pair again on the new box.
