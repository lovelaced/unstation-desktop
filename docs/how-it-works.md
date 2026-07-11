# How it works

This is the middle-length explanation: enough to really understand the design, without the
wire-level detail. If you want the plain version, read the [FAQ](faq.md). If you want the exact
formats, read the [Protocol](protocol.md).

## The problem, and the answer

Normal live streaming sends your video to a company's servers, which then send it to viewers. That
company is a single point of control: it can throttle the stream, take it down, or fail. It's also a
single point of cost, which is why a stream that suddenly gets popular can fall over.

Unstation removes the middle. Video goes directly from the broadcaster to viewers, and from viewers
to each other, over encrypted peer-to-peer connections. There's no server to take down and no
operator to lean on. And because every viewer also helps pass the stream along, more viewers means
more capacity, not less.

Going serverless brings two hard problems. First, with no central directory, how does a viewer find a
broadcaster and set up a direct connection? Second, with untrusted strangers relaying the video, how
do you know what you're watching wasn't tampered with? Unstation's whole design is the answer to
those two questions.

## The cast

- **The broadcaster** captures video and signs it with a key only they hold.
- **Viewers** pull the video, verify it, play it, and re-share it to other viewers.
- **Relays** are optional volunteer computers that lend bandwidth to bridge people who can't connect
  directly. They carry streams but can never watch or change them. See [Run a relay](run-a-relay.md).
- **The noticeboard** is a public, permissionless blockchain (a Polkadot People chain). Nobody owns
  it, anyone can post to it after proving they're a real person, and it's where broadcasters announce
  themselves and where the encrypted connection-setup messages are posted. Video never touches it.
- **The backup** is a second chain (Bulletin) that holds a durable, signed copy of each stream's
  identity, so a viewer can still verify a broadcaster even if they arrive late.

## The journey of a broadcast

Every second or so of video makes the same trip:

```
  Broadcaster                        The noticeboard                     Viewer
  (OBS / WHIP / phone)               (a public blockchain)               (app)
       |                                    |                              |
  1. capture --> CMAF video chunks          |                              |
       |         (about 1 second each)      |                              |
  2. fingerprint each chunk,                |                              |
       |  sign the manifest + live edge     |                              |
  3. announce ----------------------------> | presence: "I'm live, here"   |
       |    manifest + first chunk          |                              |
       |      --> Bulletin (backup) --------|----- 4. sealed handshake ----|
       |<-------------- sealed SDP/ICE -----|         (find each other)    |
       |                                    |                              |
       +======== 5. direct WebRTC connection, video flows here ==========>|
                    (never through the noticeboard)                        |
                 viewer verifies every chunk, then re-shares --------------+
```

**1. Capture and package.** The broadcaster's video comes in from OBS (over a local RTMP ingest),
from OBS's newer WHIP output, or from a phone camera. Whichever the source, it's packaged into small
standard video chunks (CMAF/fMP4), about one second each, aligned so each one can be decoded on its
own.

**2. Fingerprint and sign.** Each chunk is run through a hash function, and that hash is its
permanent name (its content ID): change one byte and the name changes. The broadcaster keeps a
signed running list, the *live edge*, mapping "chunk number 42" to its content ID, and signs a small
*manifest* that establishes their identity for this stream. Both signatures use a key only the
broadcaster holds.

**3. Announce.** The broadcaster posts a tiny presence note to the noticeboard so viewers can find
it by name, and writes the signed manifest plus the first chunk to the Bulletin backup chain, so a
viewer who arrives later can still verify the broadcaster from a durable source.

**4. Connect.** When a viewer wants in, their app and the broadcaster's app exchange the network
details a direct connection needs (this is standard WebRTC signaling: SDP and ICE candidates). That
exchange goes through the noticeboard, so it's always sealed: encrypted specifically to the other
party. An onlooker sees that two pseudonyms exchanged sealed messages, never the addresses inside.

**5. Pull, verify, play, re-share.** With a direct connection open, the viewer starts pulling chunks,
not just from the broadcaster but from any peer that has them. For each chunk it fetches, it computes
the hash itself and checks it against the signed live edge. A match means it's genuine, so it feeds a
tiny local video server that the player reads. A mismatch means the chunk is thrown away and the peer
that sent it is penalized. Verified chunks are then offered to other viewers, so the stream spreads
through the crowd.

The upshot: the video path is entirely peer-to-peer and cryptographically verified end to end. The
blockchain is only ever a bulletin board for "here's how to reach me" and the sealed handshakes.

## Finding a stream without a directory

There is deliberately no list of streams anywhere. A stream's location on the noticeboard is derived
by hashing its name, so anyone who knows the name computes the same location and finds the same
broadcast, and nobody can browse or enumerate what exists. An **unlisted** stream just adds a long
random code to its name, making that location unguessable, so it can only be reached by someone with
the full link.

## How the crowd shares the load

Once a viewer is connected to a few peers, the app runs a piece picker that decides, moment to
moment, which chunk to fetch from which peer. It weighs how urgent each chunk is (how close its
play-time deadline is), how rare it is (how few peers have it), and how each peer has been performing
(measured speed, round-trip time, and a reputation score). Chunks racing a deadline are fetched from
the fastest peers that have them, with a backup request in flight; chunks further out are fetched
rarest-first to spread copies through the swarm.

Peers advertise what they have and gossip about other peers they know, so the mesh keeps itself
stitched together without anyone consulting a central list. Each peer also has an upload budget and
shares its spare capacity fairly, favoring peers that reciprocate.

## Where relays fit

Two things relay for you. Any reachable viewer can automatically volunteer a slice of its upload to
help peers who can't connect directly (you can turn this off). And dedicated **open relays**,
run by volunteers on servers, advertise spare capacity on a global rendezvous point. When a
broadcaster needs help reaching everyone, it *recruits* relays: it sends each one a signed,
sealed request naming its stream, the relay verifies the broadcaster's signature before lifting a
finger, and then joins in and starts carrying the stream. This is how Unstation replaces the
"TURN server" that peer-to-peer systems usually need, without anyone having to operate one.

Recruited relays are also what makes **hiding the broadcaster's address** work: a broadcaster can
choose to serve everyone through its relays, so ordinary viewers never connect to it directly and
never learn its address. It only accepts connections from the specific relays it recruited and
verified, not from anyone who merely claims to be one. See [Security](security.md) for why that
distinction matters.

## The fast path

Verified peer-to-peer delivery lands video in a few seconds, which is right for most broadcasts. For
cases that need to be quicker, there's an optional sub-two-second path: for invited, trusted viewers,
the broadcaster can send video directly as a real-time media stream, skipping the per-chunk
verification to save time. It's opt-in and invite-gated, and the verified stream keeps running
underneath as an automatic fallback. Because it isn't verified, it's a deliberate speed-for-trust
trade, spelled out in [Security](security.md#out-of-scope).

## The trust chain, in one paragraph

Everything rests on one key. The broadcaster's signing key is tied to a proof of personhood, signs
the manifest and the live edge, and can't be forged. The live edge names every chunk by a hash that
anyone can recompute. So a viewer can follow an unbroken chain from "a real person's key" to "this
exact chunk of video," with untrusted relays and peers in the middle who can pass data along but
never alter it or forge the signatures. That's what makes it safe to accept video from strangers.

Ready for the exact formats? Continue to the [Protocol reference](protocol.md).
