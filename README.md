<div align="center">

# Unstation

*Broadcast and watch live video peer-to-peer — no servers, no CDN, no central point of failure.*

![License](https://img.shields.io/badge/license-AGPL--3.0-blue?style=flat-square)
![Platform](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-lightgrey?style=flat-square)
![Status](https://img.shields.io/badge/status-experimental-orange?style=flat-square)

<!-- TODO: hero screenshot of the live stage (video playing with the "LIVE · P2P · N peers" status line).
     Capture with Kap or Xnapper on macOS, save to assets/screenshots/live-stage.png, and replace this
     comment with:
     <img src="assets/screenshots/live-stage.png" alt="Unstation playing a live stream over the peer-to-peer mesh" width="720"> -->

</div>

---

Unstation is a desktop app for live streaming with no infrastructure in the middle. You broadcast
from OBS (or built-in screen/camera capture); viewers find your stream and pull the video directly
from each other over a WebRTC mesh. Peers relay to peers, so the more people watch, the more capacity
there is. There is no origin server, no CDN, and no relay to operate or take down — discovery and
signaling ride the Polkadot statement store, and a durable copy lives on the Bulletin chain.

## Features

- **No servers in the path** — viewers exchange video over direct WebRTC data channels. Peer discovery and the initial handshake ride the Polkadot statement store; nothing is hosted, nothing is operator-run.
- **Publish from OBS in seconds** — the app exposes a standard local RTMP ingest, so any encoder works. Point OBS at it and go live, or use the built-in screen/camera capture.
- **Every segment cryptographically verified** — each chunk is content-addressed with `blake2b256` and the stream is signed with the publisher's key, so a malicious peer can't slip in fake video.
- **Bandwidth that scales with the crowd** — a deadline-aware piece-picker pulls each segment from the fastest peer that has it. When peers can't keep up it leans on a durable floor instead of stalling.
- **Scan-once sign-in** — prove you're a person by scanning a QR code with your Polkadot app. Your keys never leave your phone; the desktop only ever holds metered, revocable slot keys.
- **Lend bandwidth** — flip on Seed Mode to turn your desktop into a volunteer seed/relay, capped by a budget you set.
- **One Rust core, fully simulated** — the mesh engine is IO-agnostic and runs thousands of virtual peers under a deterministic, seeded simulator, so behavior is reproducible and CI-gated.

## Quick start

<details>
<summary>Prerequisites</summary>

- **Rust** (stable) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Node + pnpm** — `npm i -g pnpm`
- **ffmpeg** — required to publish (RTMP → CMAF). `brew install ffmpeg` on macOS.
- **UserAgent Kit SDK** — checked out next to this repo (the desktop app depends on it by path):

  ```bash
  # alongside unstation-desktop/
  git clone <useragent-kit-url> ../useragent-kit
  ```

</details>

```bash
# Run the desktop app (publisher + viewer + seed in one)
cd desktop
pnpm install
pnpm tauri dev
```

Build and test the mesh engine on its own (no GUI, no chain):

```bash
cargo test --workspace      # engine + deterministic simulator + scenarios
cargo bench                 # criterion benchmarks
```

## Usage

### Watch a stream

Open the app, sign in once with your Polkadot app, then enter a stream name. Unstation resolves it,
discovers the publisher on the statement store, connects over WebRTC, and plays the verified video.
The status line shows whether you're on the mesh (`LIVE · P2P · N peers`) or leaning on the durable floor.

<!-- TODO: screenshot of the Watch screen — capture to assets/screenshots/watch.png -->

### Go live from OBS

1. In the app, choose **Go Live** — this opens the local RTMP ingest.
2. In OBS: **Settings → Stream → Service: Custom**, Server `rtmp://127.0.0.1:21935/live`, Stream Key `unstation`.
3. **Start Streaming.**

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

### 1. Build the DMG (GitHub Actions)

One-time setup in **Settings → Secrets and variables → Actions**:

- Add secret **`USERAGENT_KIT_TOKEN`** — a GitHub personal access token with read access to the
  UserAgent Kit SDK repo (the desktop app depends on it).
- Optional variables **`USERAGENT_KIT_REPO`** / **`USERAGENT_KIT_REF`** if the SDK isn't at
  `paritytech/useragent-kit@main`.

Then in **Actions → "Release macOS DMG" → Run workflow**, type a version tag (e.g. `v0.1.0`)
and run it. The workflow creates the tag, builds the universal `.dmg`, writes user-facing release
notes (with a collapsed "Technical details" section), and publishes a **GitHub Release** with the
`.dmg` attached. Download the `.dmg` from the release page (or from the run's build artifact).

### 2. Install on both Macs

Open the `.dmg` and drag **Unstation** into Applications. The build is unsigned, so Gatekeeper
blocks the first launch — clear the quarantine flag once:

```bash
xattr -dr com.apple.quarantine /Applications/Unstation.app
```

(Or right-click the app → **Open** → **Open**.)

### 3. Stream between them

Both Macs must be on the **same Wi-Fi/LAN** and have **internet access** — discovery and the
connection handshake ride the Polkadot statement store; only the video itself is direct
peer-to-peer. The **publishing** Mac also needs **ffmpeg** (`brew install ffmpeg`).

**Mac A — publish:**

1. Launch Unstation and sign in (scan the QR with your Polkadot app).
2. Choose **Go Live** and name the stream (e.g. `lan-test`).
3. Point OBS at `rtmp://127.0.0.1:21935/live` (Stream Key `unstation`) and **Start Streaming**.

**Mac B — watch:**

1. Launch Unstation and sign in.
2. Choose **Watch** and enter the same name (`lan-test`).

**Success looks like:** Mac B plays the video and the status line reads `LIVE · P2P · 1 peer`.
When macOS prompts to allow incoming network connections, click **Allow** (the direct WebRTC link
needs it). If B never connects, confirm both are signed in and pointed at the same statement-store
network.

## How it works

Each live segment flows through one pipeline:

1. **Ingest** — ffmpeg accepts RTMP from OBS and packages it into LL-CMAF/fMP4 segments.
2. **Address & sign** — every segment is hashed (`blake2b256`); the publisher signs the manifest and live edge with its `sr25519` key.
3. **Announce** — the publisher posts presence and a live-edge manifest to the Polkadot statement store; viewers discover it by name.
4. **Connect** — viewer and publisher exchange SDP/ICE over the statement store, then open a direct WebRTC link (a reliable `ctrl` channel + an unreliable `bulk` channel).
5. **Pull, verify, play** — a deadline-aware picker requests segments from the best peer, verifies each against its content hash, and feeds a localhost HLS server that the player reads.

The engine is split into small, IO-agnostic crates so it can run identically in production and in the simulator:

| Crate | Role |
|-------|------|
| `unstation-core` | The mesh engine — deadline picker, buffer maps, content-addressed store, SCALE wire protocol, reassembly + verify. Traits + injected clock, no IO. |
| `transport-libdc` | Real WebRTC data-channel transport (libdatachannel): two channels per peer, trickle ICE. |
| `unstation-chain` | Discovery, SDP-over-statement signaling, and the live-edge manifest over the Polkadot statement store. |
| `unstation-session` | Orchestrator — ties signaling and transport into the engine for the publish/watch bootstrap. |
| `hls-server` | Localhost HLS re-server that feeds verified segments to the player. |
| `infra/segmenter` | The ffmpeg RTMP → CMAF segmenter (the OBS ingest). |
| `unstation-node` | Headless seed/relay, also embedded for desktop Seed Mode. |
| `desktop/` | Tauri app (Rust shell + web UI): Watch / Go Live / Seed. |

## Status

Experimental and desktop-first. Real two-peer publish/watch over WebRTC works; signed-manifest trust,
the Bulletin durable floor, Seed Mode, and NAT traversal are on the roadmap. It reuses unaudited Parity
prototypes as references — measure live chain limits before relying on it for an event.

## Contributing

Issues and pull requests are welcome. Run `cargo test --workspace` before opening a PR; the deterministic
simulator must stay green.

## License

[AGPL-3.0-or-later](LICENSE).
