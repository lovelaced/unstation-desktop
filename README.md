<div align="center">

# Unstation

*Broadcast and watch live video peer-to-peer — no servers, no CDN, no central point of failure.*

![Platform](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux%20%C2%B7%20Android-lightgrey?style=flat-square)
![License](https://img.shields.io/badge/license-AGPL--3.0-blue?style=flat-square)
[![CI](https://img.shields.io/github/actions/workflow/status/lovelaced/unstation-desktop/ci.yml?style=flat-square&label=ci)](https://github.com/lovelaced/unstation-desktop/actions/workflows/ci.yml)
![Status](https://img.shields.io/badge/status-experimental-orange?style=flat-square)

<!-- TODO: hero screenshot of the live stage (video playing with the "LIVE · P2P · N peers" status line).
     Capture with Xnapper or CleanShot on macOS, save to assets/screenshots/live-stage.png, and replace
     this comment with:
     <img src="assets/screenshots/live-stage.png" alt="Unstation playing a live stream over the peer-to-peer mesh" width="720"> -->

</div>

---

Unstation is a desktop app for live streaming with no infrastructure in the middle. You broadcast
from OBS (or the built-in ingest); viewers pull the video directly from you — and from each other —
over a WebRTC mesh, so the more people watch, the more capacity there is. Discovery and the
connection handshake ride the Polkadot statement store; a durable copy of the stream's identity
lives on the Bulletin chain. There is no origin server, no CDN, and no relay operator to take down.

There's also an [Android app](https://github.com/lovelaced/unstation-android) that watches the same
streams and broadcasts straight from the phone camera.

## Features

- **No servers in the path** — viewers exchange video over direct WebRTC data channels. Peer discovery and the initial handshake ride the Polkadot statement store; nothing is hosted, nothing is operator-run.
- **Publish from OBS in seconds** — the app exposes a standard local RTMP ingest, so any encoder works. Point OBS at it and go live; a guided setup panel appears if the encoder hasn't connected.
- **Invite your friends with a link** — every stream gets an `unstation://watch/<name>` link plus a QR code. Opening it lands straight in the stream; typing the name still works.
- **Every segment cryptographically verified** — each chunk is content-addressed with `blake2b256` and the stream is signed with the publisher's key, so a malicious peer can't slip in fake video. Peers that serve forged bytes are scored down and banned.
- **Honest, legible states** — joining, catching up, "can't reach anyone", and "the broadcast ended" are real states driven by the engine, with retry paths — never an eternal spinner. Publishers get a live dashboard: preflight checks, viewers, encoder and uplink bitrate.
- **Bandwidth that scales with the crowd** — a deadline-aware piece-picker pulls each segment from the fastest peer that has it, and viewers that prove reachable automatically volunteer as relays for peers stuck behind NAT.
- **Scan-once sign-in** — prove you're a person by scanning a QR with your Polkadot app. Your keys never leave your phone; the desktop only ever holds a small, revocable network pass.
- **One Rust core, deterministically tested** — the mesh engine is IO-agnostic and runs hundreds of virtual peers under a seeded simulator, with adversarial-peer and churn suites gating CI.

## Quick start

<details>
<summary>Prerequisites</summary>

- **Rust** (stable) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Node + pnpm** — `npm i -g pnpm`
- **ffmpeg** — required to publish (RTMP → CMAF). `brew install ffmpeg` on macOS.
- **The chain SDK** — a private Polkadot Rust SDK this app builds against, checked out as
  `../useragent-kit` (a sibling of this repo). Ask the maintainers for access.

</details>

```bash
# Run the desktop app (publisher + viewer in one)
cd desktop
pnpm install
pnpm tauri dev
```

Build and test the mesh engine on its own (no GUI, no chain):

```bash
cargo test --workspace      # engine + deterministic simulator + adversarial/churn suites
cargo bench                 # criterion benchmarks
```

Or install a prebuilt `.dmg` from [Releases](https://github.com/lovelaced/unstation-desktop/releases).

## Usage

### Watch a stream

Open an invite link — or open the app, sign in once with your Polkadot app, and type the stream
name. Unstation resolves it, verifies the publisher's signed manifest, connects over WebRTC, and
plays the verified video. The status line shows the truth: `LIVE · P2P · N peers`, catching up,
or an honest "can't reach anyone" with the app still retrying underneath.

### Go live from OBS

1. In the app, choose **Go Live** and name the stream — this opens the local RTMP ingest.
2. In OBS: **Settings → Stream → Service: Custom**, Server `rtmp://127.0.0.1:21935/live`, Stream Key `unstation`.
3. **Start Streaming.** The console flips to LIVE when real fragments arrive, and shows your
   preflight (`identity ✓ · announced ✓ · encoder ✓`), viewers, and bitrates.
4. Share the invite link (or QR) from the console.

No OBS handy? `scripts/mock-obs.sh` is a faithful ffmpeg stand-in:

```bash
scripts/mock-obs.sh                 # moving test pattern + tone → the default ingest
scripts/mock-obs.sh -i clip.mp4     # stream a file (looped) instead
scripts/mock-obs.sh -t 10           # one-shot 10 s (handy in tests)
```

The full real-media path (OBS-style RTMP → segmenter → mesh → HLS → `ffprobe`-verified playback)
is covered by `cargo test -p hls-server --test go_live -- --ignored`.

## Test on a second Mac over your LAN

Unstation is peer-to-peer, so the real test is two machines. The
[`Release macOS DMG`](.github/workflows/release-macos.yml) workflow produces a universal
(Apple Silicon + Intel) `.dmg` you can install on a second Mac.

### 1. Build the DMG

If CI has credentials for the private chain SDK (**Settings → Secrets and variables → Actions**:
secret `SDK_TOKEN`, variable `SDK_REPO`, optional `SDK_REF`), run
**Actions → "Release macOS DMG" → Run workflow** with a version tag (e.g. `v0.1.0`) — it builds
the universal `.dmg` and publishes a GitHub Release with notes.

> **No CI credential?** Build on your own machine — you already have the SDK checked out next to
> this repo:
>
> ```bash
> scripts/release-macos.sh            # build only → prints the .dmg path to AirDrop to the other Mac
> scripts/release-macos.sh v0.1.0     # build + cut a GitHub release (needs `gh`, logged in)
> ```
>
> Same universal `.dmg` and the same git-cliff notes, no tokens involved.

### 2. Install on both Macs

Open the `.dmg` and drag **Unstation** into Applications. The build is unsigned, so Gatekeeper
blocks the first launch — clear the quarantine flag once:

```bash
xattr -dr com.apple.quarantine /Applications/Unstation.app
```

(Or right-click the app → **Open** → **Open**.)

### 3. Stream between them

Both Macs need **internet access** — discovery and the connection handshake ride the Polkadot
statement store; only the video itself is direct peer-to-peer. The **publishing** Mac also needs
**ffmpeg** (`brew install ffmpeg`).

**Mac A — publish:**

1. Launch Unstation and sign in (scan the QR with your Polkadot app).
2. Choose **Go Live** and name the stream (e.g. `lan-test`).
3. Point OBS at `rtmp://127.0.0.1:21935/live` (Stream Key `unstation`) and **Start Streaming**.

**Mac B — watch:**

1. Launch Unstation and sign in.
2. Choose **Watch** and enter the same name (`lan-test`).

**Success looks like:** Mac B plays the video and the status line reads `LIVE · P2P · 1 peer`.
When macOS prompts to allow incoming network connections, click **Allow** (the direct WebRTC link
needs it). Cross-network pairs use STUN and volunteer relays; for networks where nothing else
works, operator-provided TURN servers can be supplied via `UNSTATION_TURN`.

## How it works

Each live segment flows through one pipeline:

1. **Ingest** — ffmpeg accepts RTMP from OBS and packages it into CMAF/fMP4 segments, keyframe-aligned at 1 s for low latency.
2. **Address & sign** — every segment is hashed (`blake2b256`); the publisher signs the manifest and live edge with its `sr25519` key.
3. **Announce** — the publisher posts presence to the Polkadot statement store and anchors its signed manifest + init segment on the Bulletin chain; viewers discover it by name.
4. **Connect** — viewer and publisher exchange SDP/ICE over the statement store, then open a direct WebRTC link (a reliable `ctrl` channel + an unreliable `bulk` channel). A connection maintainer redials with backoff and reacts to drops instantly.
5. **Pull, verify, play** — a deadline-aware picker requests segments from the best peer (weighted by measured throughput, latency, and reputation), verifies each against its content hash, and feeds a localhost HLS server that the player reads. Segments then reshare peer-to-peer, and the live edge propagates by signed in-mesh gossip.

The engine is split into small, IO-agnostic crates so it runs identically in production and in the simulator:

| Crate | Role |
|-------|------|
| `unstation-core` | The mesh engine — deadline picker, buffer maps, content-addressed store, SCALE wire protocol, reassembly + verify, peer scoring/bans, bounded-memory hardening. Traits + injected clock, no IO. |
| `transport-libdc` | Real WebRTC data-channel transport (libdatachannel): two channels per peer, trickle ICE, degree caps. |
| `unstation-chain` | Discovery, SDP-over-statement signaling, the live-edge manifest, and the Bulletin origin-of-record. |
| `unstation-session` | Orchestrator — dial pacing/backoff, the connection maintainer, and the publish/watch bootstrap. |
| `unstation-app` | The Tauri command/event layer shared by the desktop and Android shells. |
| `hls-server` | Localhost HLS re-server that feeds verified segments to the player. |
| `infra/segmenter` | The ffmpeg RTMP → CMAF segmenter (the OBS ingest) and the on-device CMAF muxer. |
| `unstation-node` | Headless seed/relay scaffold (volunteer relaying currently happens in-app: reachable viewers auto-promote). |
| `desktop/` | Tauri app (Rust shell + web UI): Watch / Go Live / Settings. |

## Status

Experimental. Working today: two-machine publish/watch over real WebRTC, signed-manifest trust,
camera broadcasting from [Android](https://github.com/lovelaced/unstation-android), invite links,
peer scoring with bans, volunteer relaying, and the Bulletin anchor for the stream's identity.
On the roadmap: a dedicated seed mode with its own budget controls, segment-level durable
fallback, push-based signaling, and a sub-2-second WebRTC media path. It reuses unaudited
prototypes as references — measure live chain limits before relying on it for an event.

## Contributing

Issues and pull requests are welcome. Before opening a PR run `scripts/test-all.sh` (or at least
`cargo test --workspace`) — the deterministic simulator, the adversarial/churn suites, and the
engine coverage gate (≥ 90%) must stay green.

## License

[AGPL-3.0-or-later](LICENSE).
