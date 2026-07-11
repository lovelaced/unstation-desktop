# Security

This page is the honest version. It describes what Unstation actually protects, what it deliberately
doesn't, and exactly what each party in the system can see. It's written for readers who want to
understand the design and decide how much to trust it. If you just want the plain answers, the
[FAQ](faq.md) is friendlier.

The short version: **content integrity and connection confidentiality are strong; metadata and
network-level anonymity are not the goal.** Unstation makes it very hard to feed you fake video or to
passively harvest who-is-talking-to-whom from the public record. It does not hide your IP address
from a peer you connect to, and it is not a replacement for a VPN or Tor.

Unstation is unaudited. Nothing here has been through a formal third-party security review.

## How you know the video is real

There is no trusted server vouching for anything, so trust comes entirely from cryptography.

Every broadcaster has a signing key that only they hold, tied to a proof of personhood. When they go
live, they publish a small signed document (the *manifest*) that establishes their identity for that
stream, and they continuously sign the *live edge*, the running list of which chunks exist and what
each one's fingerprint is.

Every chunk of video is addressed by its own content fingerprint. Your app fetches a chunk from
whatever peer has it, computes the fingerprint itself, and checks it against the broadcaster's signed
edge. If it doesn't match, the chunk is thrown away and the peer that served it is scored down and
eventually banned. A relay or a malicious viewer can refuse to send you data, but it cannot send you
*different* data without being caught. This is what lets untrusted strangers safely carry a stream.

An impostor who copies a stream's name can't fake the signature, because they don't have the key, so
their manifest fails verification and your app moves on.

## What's encrypted, and what isn't

**Connection setup is always encrypted.** To connect two people, their apps exchange the network
details that let them find each other (the addresses and candidates a direct connection needs). That
exchange happens through a public noticeboard, so it is always sealed: each message is encrypted to
its specific recipient using keys derived from their identities, and there is deliberately no
unencrypted fallback. If a message can't be sealed, it is dropped rather than sent in the clear. An
observer reading the noticeboard sees that two pseudonyms exchanged sealed messages, never the
addresses inside.

**Stream content depends on the stream's mode:**

- A **public** stream is not content-encrypted. It's public, so anyone who knows the name can watch.
  Integrity still holds: the video can't be forged, only read.
- An **unlisted** stream is the same as public, except its name carries a long random code, so its
  location on the noticeboard can't be found by guessing or browsing. This is protection by
  unguessability, not by encryption. Anyone with the full link has full access.
- An **invite-only** stream is encrypted end to end. Each chunk is sealed with a stream key, and the
  fingerprint everyone verifies against is the fingerprint of the *encrypted* chunk. The key travels
  only in the invite link, in the part of a URL that is never sent to any server (the fragment after
  `#`). Relays and other viewers carry sealed chunks they can verify but cannot open. Only a device
  with the invite key decrypts, and only after the chunk has been integrity-checked, just before it
  reaches the player.

## Hiding the broadcaster's address

By default, a viewer connects directly to the broadcaster, so the broadcaster's IP address is visible
to viewers, the same way it would be in any peer-to-peer call. For broadcasters who need to stay
hidden, Unstation has **origin shield**: with it on, the broadcaster refuses direct connections from
ordinary viewers and serves everyone through volunteer relays, so only the relays ever learn the
broadcaster's address.

The important detail is *how* the broadcaster decides who counts as a relay. An earlier version
trusted a self-declared "I am a relay" flag, which meant anyone could claim it and get a direct
connection, defeating the point. That is fixed: a shielded broadcaster only accepts connections from
the specific relays it recruited itself, identified by their cryptographically verified signing
accounts as recorded on the chain, never by a flag that anyone can set. A stranger who self-declares
as a relay is refused.

## What can each party see

A precise breakdown. "The noticeboard" is the public statement store; "endpoints" are the network
services clients reach it and other resources through.

| Party | Can see | Cannot see |
|-------|---------|------------|
| **Someone reading the noticeboard** | That a pseudonym posted presence for a stream; that sealed connection-setup messages were exchanged; that a broadcaster recruited relays | Any IP address; the contents of sealed messages; any video; an unlisted stream it doesn't know the name of |
| **A relay, or a peer you connect to** | Your IP address (a direct connection reveals it); the video passing through (encrypted, for invite-only streams) | Nothing that lets it forge or alter the video; the content of an invite-only stream |
| **The broadcaster** | The incoming connections, so the IP addresses of whoever connects to them directly | Any name or identity (there are no accounts); viewer IPs, if origin shield is on and viewers come through relays |
| **An endpoint operator** (chain RPC, content gateway, or STUN server) | Your IP address; which streams you're interested in (from the topics and content IDs you request); timing | The contents of sealed messages; invite-only video |

## On-chain metadata

There are no accounts, and the keys that identify you never leave your phone. Your device acts under
a per-app pseudonym, not your name, and the notes it posts expire on their own.

The honest caveat: **all of one device's activity is signed by that same pseudonym.** Someone
watching the chain over time could build a profile like "this pseudonym broadcast these streams on
these evenings," without ever learning who the pseudonym belongs to. Linking that pseudonym to a real
person would require information from somewhere else (an IP correlation, or the initial personhood
sign-in, depending on how that's implemented on the underlying chain). Per-stream unlinkable
pseudonyms are possible with the underlying key scheme and are a planned improvement, but today one
device uses one pseudonym.

Recruitment adds one more visible edge: an observer can see that a broadcaster's pseudonym sent a
sealed message to a relay's pseudonym, so who-recruited-whom is visible even though the content is
sealed.

## What are the weak points

Being honest about where pressure could be applied or where privacy is thinner than you might hope:

- **Default endpoints.** The chain and the content backup are reached through a small set of default
  network endpoints. Anyone can run their own, and the app accepts alternatives, but out of the box a
  handful of hostnames see your IP and your stream interests. This is the classic light-client
  metadata problem, and self-hosting is the real answer for the metadata-sensitive.
- **STUN.** Establishing direct connections uses public STUN servers (Google and Cloudflare by
  default) to discover your public address. Those servers see your IP. They can be overridden.
- **The participation gate.** Posting to the noticeboard requires a permission slip granted through
  proof of personhood. That's what keeps out bots, but it also means participation depends on that
  granting path working and being reachable.
- **Your IP to a direct peer.** This is inherent to peer-to-peer. Whoever you exchange video with
  learns your address. Origin shield hides the *broadcaster*, and relays sit between viewers and the
  broadcaster, but the relay you use still sees you. A VPN is the tool for hiding your IP from a
  counterparty; Unstation does not build one in, because the low-latency video path needs UDP that
  anonymity networks like Tor won't carry well.
- **Pseudonym linkage over time**, described above.
- **App distribution.** However good the protocol is, you still have to trust the build you're
  running. Build from source if that matters to you.

## Out of scope

Things Unstation deliberately does not try to solve:

- **Anonymity from a global passive observer.** Someone who can watch all network traffic everywhere
  can do timing and traffic analysis. Defending against that is a different (and much harder) design.
- **Hiding your IP from the peers you exchange video with.** See above; use a VPN if you need this.
- **The fast path is unverified by design.** There is an optional sub-second mode for invited,
  trusted viewers that skips the per-chunk verification to cut latency. It's publisher-direct and
  invite-gated, and the verified stream keeps running underneath as the fallback. Don't rely on the
  fast path's contents being verified; that's the trade it makes for speed.
- **A formal audit.** There hasn't been one.

## Reporting a vulnerability

If you find a security issue, please report it privately to the maintainers rather than opening a
public issue, and give a reasonable window to fix it before disclosure.
