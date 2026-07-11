<div align="center">

# Unstation

*Live video that no one can switch off. Watch and broadcast peer to peer, with no server in the middle.*

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

Most live streaming runs through a company's servers. That company can throttle it, take it down, or
just switch it off. Unstation has no servers. The video goes straight from the person broadcasting to
the people watching, and from viewer to viewer, so there is nothing in the middle to pull the plug on.

The more people watch, the more capacity there is, because everyone watching also helps pass the
stream along. You sign in once by scanning a code with the Polkadot app on your phone. There are no
accounts, no email, and no company holding your details.

There is a desktop app for macOS, Windows, and Linux, and an
[Android app](https://github.com/lovelaced/unstation-android) that watches the same streams and can
broadcast straight from your phone camera.

## What you can do

- **Watch.** Open an invite link, or type a stream's name. Unstation finds whoever is broadcasting,
  checks that the video really came from them, and plays it.
- **Broadcast.** Point OBS (or any streaming app) at Unstation and go live, or broadcast from your
  phone camera. Share a link and people can watch right away.
- **Help out.** Run a small program on a spare server and it becomes a *relay*, lending its bandwidth
  so people on hard networks (phones on cell data, viewers behind strict firewalls) can still connect.
  See [Run a relay](docs/run-a-relay.md).

## Why it's different

- **No one in the middle.** The video travels directly between people over an encrypted connection.
  Nothing is hosted on a server, so there is no operator to pressure and nothing central to take down.
- **You can't be fed fake video.** Every piece of the stream is fingerprinted and signed by the
  broadcaster, so if a bad actor tries to slip in altered video, your app rejects it automatically.
- **Private by default.** The addresses that connect people are always encrypted in transit. You can
  make a stream *unlisted* (only people with the link can find it) or *invite only* (end to end
  encrypted, so even the relays passing it along can't watch it).
- **It gets stronger with a crowd.** Instead of buckling under load like a single server, a bigger
  audience means more people sharing the stream, so popular broadcasts hold up better, not worse.
- **Honest about what's happening.** Connecting, catching up, "can't reach anyone right now", "the
  broadcast ended" are real, plainly-worded states, never an endless spinner that hides a problem.

## How it works, in a nutshell

There is no central directory of streams. Instead, Unstation uses a public, permissionless
noticeboard (the Polkadot statement store) where broadcasters post a tiny "I'm live, here's how to
reach me" note, and viewers post encrypted notes to set up a direct connection. Once two people are
connected, the video itself flows straight between them, never touching the noticeboard.

Each broadcaster signs their stream with a key that only they hold, and every chunk of video is
addressed by its own fingerprint, so your app can prove that what it plays is exactly what the
broadcaster sent. Relays and other viewers can pass the video along, but they can never change it.

For the full picture, see [How it works](docs/how-it-works.md). For the wire-level details, the
[Protocol](docs/protocol.md) reference. For the honest security story (what's protected, what leaks,
and the threat model), [Security and FAQ](docs/security.md).

## Install

Download a build from [Releases](https://github.com/lovelaced/unstation-desktop/releases), or run
it from source:

```bash
cd desktop
pnpm install
pnpm tauri dev
```

Building the full app needs a few tools and access to a private Polkadot SDK. The engine on its own
builds and tests with no chain and no GUI. See
[Contributing and building from source](docs/contributing.md) for the details.

## Documentation

| Doc | For |
|-----|-----|
| [How it works](docs/how-it-works.md) | The whole system in plain but technical language, with a diagram |
| [Protocol](docs/protocol.md) | Wire formats, the chain layer, the mesh, the trust chain |
| [Security](docs/security.md) | The threat model, what's protected, what leaks, honest limits |
| [FAQ](docs/faq.md) | Is it private, is it legal, what do I need, what does it cost |
| [Run a relay](docs/run-a-relay.md) | Lend bandwidth from a spare server in one command |
| [Contributing](docs/contributing.md) | Repo layout, building the app and the relay, running the tests |

The docs are also published as a [site](https://lovelaced.github.io/unstation-desktop/).

## Status

Experimental and unaudited. Working today: broadcasting and watching between machines over real
peer-to-peer connections, signed-stream trust so video can't be forged, invite links, unlisted and
end-to-end-encrypted streams, hiding the broadcaster's address behind volunteer relays, a durable
backup copy of each stream's identity, a sub-second low-latency path for trusted viewers, and
open relays that anyone can run on a spare server in one command.

It leans on unaudited prototype components and a public test network. Measure the live limits before
you rely on it for something that matters.

## Contributing

Issues and pull requests are welcome. Before opening a PR, run `scripts/test-all.sh` (or at least
`cargo test --workspace`): the deterministic simulator, the adversarial and churn suites, and the
engine coverage gate must stay green. See [Contributing](docs/contributing.md) to get set up.

## License

[AGPL-3.0-or-later](LICENSE).
