# Watch what can't be taken down

Unstation is live video with no company and no servers in the middle. The video goes straight from
the person broadcasting to the people watching, and from viewer to viewer, so there is nothing
central to throttle, take down, or switch off. The more people watch, the more capacity there is,
because everyone watching also helps pass the stream along.

There's a desktop app for macOS, Windows, and Linux, and an
[Android app](https://github.com/lovelaced/unstation-android) that watches and broadcasts from your
phone camera.

## Start here

- **Just want to understand it?** [How it works](how-it-works.md) walks through the whole system in
  plain but technical language, with a diagram.
- **Curious about privacy or safety?** [Security](security.md) covers the threat model: what's
  protected, what leaks, and to whom. [FAQ](faq.md) answers the everyday questions.
- **Want to help?** [Run a relay](run-a-relay.md) on a spare server and lend bandwidth in one
  command. A relay carries streams but can never watch or change them.
- **Building on it?** The [Protocol](protocol.md) reference has the wire formats and the chain
  layer. [Contributing](contributing.md) covers the repo and the tests.

## What makes it different

- **No one in the middle.** Video travels directly between people over encrypted connections. There
  is no origin server and no relay operator to pressure.
- **Video that can't be forged.** Every chunk is fingerprinted and signed by the broadcaster, so
  altered video is rejected automatically. Relays and other viewers can pass a stream along but never
  change it.
- **Private by design.** Connection details are always encrypted in transit. Streams can be unlisted
  (unfindable without the link) or invite-only (end to end encrypted, so even the relays can't
  watch).
- **Stronger with a crowd.** A bigger audience means more people sharing the load, so popular streams
  hold up instead of buckling.

Unstation is experimental, unaudited, and community-run, and it's free software under the AGPL-3.0.
The code is on [GitHub](https://github.com/lovelaced/unstation-desktop).
