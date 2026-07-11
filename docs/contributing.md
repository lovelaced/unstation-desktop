# Contributing and building from source

Issues and pull requests are welcome. This page covers the repo layout, how to build each piece,
and how to run the tests.

## The two repositories

Unstation lives in two repos plus a shared SDK:

- **`unstation-desktop`** (this repo) holds the mesh engine, the chain integration, the relay, and
  the desktop app. The web UI under `desktop/src` is the single source of truth for both the desktop
  and the Android front ends.
- **`unstation-android`** wraps that same `desktop/src` UI in an Android shell and adds phone camera
  capture. It single-sources this repo's UI, so the two must sit under the same parent folder.
- **`useragent-kit`** is a Polkadot Rust SDK (the statement store, wallet, and Bulletin client) that
  the chain-facing crates build against. It is a private dependency checked out as a sibling
  (`../useragent-kit`). The mesh engine itself does not depend on it, so most of the codebase builds
  and tests without it.

## Repo layout

| Crate / folder | Role |
|----------------|------|
| `unstation-core` | The mesh engine: the deadline-aware piece picker, the content-addressed segment store, the wire protocol, reassembly and verification, peer scoring and bans, and bounded-memory hardening. Pure and IO-agnostic, with an injected clock, so it runs identically in production and in the simulator. |
| `transport-libdc` | The real WebRTC data-channel transport, built on libdatachannel. |
| `unstation-chain` | Discovery, encrypted signaling, the live-edge manifest, volunteer and recruitment records, and the Bulletin durable copy. Talks to the SDK. |
| `unstation-session` | The orchestrator: dial pacing and backoff, the connection maintainer, origin-shield, and the publish and watch bootstrap. |
| `unstation-app` | The command and event layer shared by the desktop and Android shells. |
| `unstation-node` | The headless volunteer relay, with a one-line installer. |
| `hls-server` | A localhost HLS server that feeds verified segments to the player. |
| `infra/segmenter` | The ffmpeg RTMP-to-CMAF segmenter and the on-device muxer. |
| `desktop/` | The Tauri app: a Rust shell plus the web UI (Watch, Go Live, Settings). |

The engine workspace deliberately **excludes** the SDK-backed crates (`unstation-chain`,
`unstation-session`, `unstation-app`, `unstation-node`, and the Tauri shell) so that
`cargo test --workspace` stays fast and needs no chain. Build those crates from their own
directories.

## Building the engine (no chain, no GUI)

The core mesh engine needs only a Rust toolchain:

```bash
cargo test --workspace   # engine + deterministic simulator + adversarial and churn suites
cargo bench              # criterion benchmarks
```

## Building the full app

<details>
<summary>Prerequisites</summary>

- **Rust** (stable): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Node and pnpm**: `npm i -g pnpm`
- **ffmpeg** (needed to broadcast): `brew install ffmpeg` on macOS
- **The chain SDK** as `../useragent-kit`, a sibling of this repo. It is private; ask the maintainers
  for access.

</details>

```bash
cd desktop
pnpm install
pnpm tauri dev
```

To build a distributable macOS `.dmg` (universal, Apple Silicon and Intel):

```bash
scripts/release-macos.sh          # build only, prints the .dmg path
scripts/release-macos.sh v0.1.0   # build and cut a GitHub release (needs the gh CLI)
```

## Building the relay

The relay is `unstation-node`. It needs the SDK sibling checkout, same as the other chain crates:

```bash
cd crates/unstation-node
cargo build --release
```

Then install your build with `sudo scripts/seed/install.sh --binary path/to/unstation-node`. See
[Run a relay](run-a-relay.md) for the operator side.

## Running the tests

`scripts/test-all.sh` runs the tiers in order:

```bash
scripts/test-all.sh           # fast tiers: engine, simulator, adversarial/churn, netsim
scripts/test-all.sh --chain   # + real end-to-end tests against a local dev chain node
scripts/test-all.sh --paseo   # + a smoke test against the public test network
```

The fast tiers need no chain and finish in seconds. `--chain` boots a local development node and
runs the real end-to-end suite (real chain, real WebRTC), including the relay recruitment flow. The
network simulator (`netsim`) replays deterministic packet loss and latency to catch protocol holes.

Before opening a pull request, run `scripts/test-all.sh` (or at least `cargo test --workspace`). The
deterministic simulator, the adversarial and churn suites, and the engine coverage gate must stay
green.

## Editing the docs

This site is plain markdown in `docs/`, built into HTML by a small script (`docs/build.py`, no
Jekyll) and published by `.github/workflows/pages.yml`. To preview your changes:

```bash
pip install markdown
python3 docs/build.py
open docs/_site/index.html
```

To add a page, create the markdown file, add a line for it to the `PAGES` list in `docs/build.py`
(which drives the nav), and un-ignore it in `.gitignore` (the repo ignores `*.md` except the
published set).

The deep design specs and research notes are kept local to the repo and are not published. The
public reference is this site: [How it works](how-it-works.md) for the architecture and
[Protocol](protocol.md) for the wire-level details.
