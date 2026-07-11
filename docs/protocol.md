# Protocol reference

The precise version. This describes the formats and rules a second implementation would need to
interoperate. For the narrative, read [How it works](how-it-works.md); for the threat model, read
[Security](security.md).

Cryptographic primitives used throughout: **BLAKE2b-256** for all hashing and content addressing,
**sr25519** for identity signatures, **X25519** for key agreement, and **XChaCha20-Poly1305** for
sealed messages and encrypted segments. Structured data is **SCALE**-encoded (the Polkadot codec).

## Layers

| Layer | Job |
|-------|-----|
| **Chain** | Discovery, sealed connection-setup, the signed live edge, durable backup. Two chains: a Polkadot People-chain *statement store* and the *Bulletin* chain. |
| **Session** | Turns names into swarms: discovery, dial pacing, the connection maintainer, origin-shield. |
| **Mesh** | The peer-to-peer engine: piece picking, the segment store, verification, peer scoring, upload budgeting, over WebRTC data channels. |
| **Media** | Ingest to CMAF segments; a localhost HLS server feeding the player; an optional real-time fast path. |

## Identity and naming

Four distinct identifiers, often confused:

- **Personhood key.** An sr25519 key tied to a proof of personhood, stable across a person's devices.
  It's the trust anchor: it signs manifests, live edges, and recruitments, and it's the `publisher`
  field a viewer verifies against. It never leaves the phone; devices act through slot keys (below).
- **Slot account.** A per-app sr25519 key the phone grants (`//allowance//statement-store//<product>`,
  and a parallel one for Bulletin). This is the key that actually signs chain writes and whose proof
  the chain verifies against its allowance. Product IDs: `unstation-live` (apps), `unstation-seed`
  (relays).
- **Peer ID.** A random 32-byte routing address, regenerated per session, decoupled from any signing
  key. Used for presence, dial targeting, and addressing sealed messages; never signs anything.
- **Stream ID.** `BLAKE2b-256(canonical_name)`. Canonicalization: trim, drop a `.dot` suffix,
  lowercase, collapse runs of non-alphanumerics to a single `-`, trim `-`, and map empty to
  `my-stream`. Because the ID is the hash of the human name, any resolver of the name reaches the
  same swarm. Unlisted streams append a ~72-bit random token to the name to make the ID unguessable.

## The chain layer

### Statement store

A Polkadot People chain. The default endpoint is **Paseo (a testnet)**,
`wss://paseo-people-next-system-rpc.polkadot.io`, overridable with `HOST_STATEMENT_STORE_WS_ENDPOINTS`.
Every write is a signed *statement* posted to a *topic* and a *channel*; the store keeps the
highest-priority statement per `(account, channel)` (last-write-wins). Statements are accepted only
from accounts that hold a statement-store allowance.

All topics are `BLAKE2b-256` over a domain string concatenated with IDs:

| Topic | Derivation | Written by | Payload |
|-------|-----------|------------|---------|
| discovery | `"disc" ‖ stream_id ‖ shard` | broadcasters and reachable relays (anchors only) | `PresenceRecord` |
| signaling | `"sig" ‖ stream_id ‖ peer_id` | any dialer | sealed SDP/ICE bundle |
| fast signaling | `"fastsig" ‖ stream_id ‖ peer_id` | fast-path peers | sealed media SDP/ICE |
| live edge | `"edge" ‖ stream_id` | broadcaster | `Vec<(Seq, content_id)>` |
| durable map | `"durable" ‖ stream_id` | broadcaster | `Vec<(Seq, Bulletin CID)>` |
| volunteers | `BLAKE2b-256("unstation/volunteers/v1")` | open relays | `VolunteerRecord` |
| recruit inbox | `"recruit" ‖ peer_id` | broadcasters | sealed `Recruitment` |

Plain viewers write **nothing** to the chain; they discover and gossip peer-to-peer instead. Sharding
exists but the shipping default is a single shard.

**Lifetime.** A statement's on-chain retention is short (about 30 seconds nominal) but statements can
linger up to about an hour. Records therefore carry their own `issued_at` and `ttl_s` and readers
age-filter on those, never trusting store retention. Because only one statement survives per
`(account, channel)`, a sender that needs to accumulate messages (like trickled ICE candidates)
re-sends the **whole bundle** each time with a strictly increasing priority, so the newest superset
wins. Delivery is push (a per-topic subscription) with polling as reconciliation.

### Bulletin

The Bulletin chain stores the durable, content-addressed copy of each stream's **signed manifest**
and **first (init) segment**, addressed by Bulletin's own preimage CID. Bulk segments are *not* on
Bulletin; the durable map on the statement store points at the sparse segments a broadcaster chooses
to upload. Writing to Bulletin requires a separate Bulletin allowance (a parallel phone-granted slot
key).

### Allowances

Posting to either chain needs an allowance, which is how personhood gates participation without a
gatekeeper. Apps get theirs during sign-in: the Polkadot phone app, over a Mobile SSO exchange,
grants the device a per-product slot signing key that already carries an allowance. Relays get theirs
by running `unstation-node pair`, a headless version of the same exchange (terminal QR, phone
approves, phone funds the relay's account, relay receives and persists its slot key). A relay
probes that its key can actually write at boot and fails loudly if not.

## Records and signatures

### PresenceRecord

Announces a participant on a discovery topic: `peer_id`, `publisher` (personhood key), upload
capacity, `ttl_s`, an optional manifest CID, a `relay` flag, and an `enc_pub` (the X25519 key others
seal to). Application `ttl_s` defaults to 30 s, clamped `[5, 300]` on read.

> Note: the `relay` flag here is self-asserted and is only a hint. Origin-shield does **not** trust
> it; see [Security](security.md#hiding-the-broadcasters-address).

### Manifest

`SignedManifest` wraps a SCALE `Manifest { stream_id, kind, codec, init_segment_cid,
target_segment_ms, ll_mode, tracks, publisher, created_at, encrypted }`, signed sr25519 under the
context string `"unstation-manifest"`. `publisher` is the personhood key. Verifying checks both that
the embedded `publisher` equals the expected anchor and that the signature is valid. The `encrypted`
flag is inside the signed bytes, so a relay can't strip it to trick a viewer into treating an
encrypted stream as plaintext.

### Live edge

`EdgeAnnounce { seq, id, sig }` where `id` is the chunk's content ID. The signed payload is
`"unstation-edge-v1" ‖ stream_id ‖ seq ‖ content_id`, sr25519 under the manifest context. The
broadcaster signs each produced chunk and gossips the announcement through the mesh; every hop
re-verifies against the publisher key before applying or re-gossiping, and re-announces periodically
for loss recovery. The chain's edge topic is a coarse fallback for viewers not yet in the mesh.

### VolunteerRecord

A version-1 frozen wire type: `{ version, peer_id (recruit-inbox address), account (personhood key),
enc_pub, caps_upload_bps, active_streams, max_streams, ttl_s, issued_at }`. A `max_streams` of 0 is a
tombstone. Announced on the global volunteers topic every 120 s with a 240 s `ttl_s`, at a
time-derived priority so a restart's fresh announcement outranks its own tombstone.

### Recruitment

`{ version, stream_id, manifest_cid, publisher, issued_at, action (Recruit or Release), sig }`, signed
sr25519 under the context `"unstation-recruit"`. The signed payload also binds the *target relay's*
peer ID, which is not on the wire (it's implied by the inbox topic), so a recruitment can't be
replayed into a different relay's inbox. Posted **sealed** to the target's recruit topic. A relay
verifies the signature and freshness (±600 s), then fetches the named manifest from Bulletin and
confirms it verifies against the recruiting publisher and names the recruited stream, before joining.

## The sealed envelope

Connection-setup messages and recruitments are sealed with static-static X25519 ECDH. Each identity
derives a long-lived X25519 keypair from its identity secret (domain-separated), advertised as
`enc_pub` in presence. To seal to a recipient: ECDH to a shared secret, derive the AEAD key as
`BLAKE2b-256("unstation-seal-v1" ‖ shared)`, and XChaCha20-Poly1305 with a fresh random 24-byte nonce.

```
envelope = 0x02 ‖ sender_x25519_pub(32) ‖ nonce(24) ‖ ciphertext+tag
```

For signaling, the plaintext body is `from_peer_id(32) ‖ encoded(SignalMsg)`. There is **no plaintext
fallback**: if the recipient's key isn't known, the message is dropped rather than sent in the clear,
because SDP and ICE carry IP addresses. A legacy unsealed envelope tag is rejected outright (no
downgrade).

## Segments and content addressing

Media is CMAF/fMP4: a ~1 KB init segment once, then media chunks (~1 second for RTMP ingest, ~250 ms
parts for WHIP and phone camera). A chunk's content ID is `BLAKE2b-256(chunk_bytes)`, and chunks are
referenced by a monotonic `Seq`. The `Seq → content_id` map from the signed manifest and live edge is
the authenticated availability table.

For **encrypted (invite-only)** streams, each chunk (and the init segment) is sealed with the stream
key using XChaCha20-Poly1305 under the domain `"unstation-segment-v1"` (`nonce(24) ‖ ct+tag`), and the
content ID is the hash of the **ciphertext**. Relays, the mesh, and Bulletin therefore carry and
verify ciphertext; only a device with the stream key (carried solely in the invite link's URL
fragment) decrypts, and only after the hash check, just before the player.

## The mesh

### Transport

WebRTC via libdatachannel, two data channels per peer: `ctrl` (reliable, ordered) and `bulk`
(unordered, no retransmits). Segment data is chunked at **16 KiB** and hand-framed. Bulk sending is
paced (target ~64 KiB in flight, 3 MiB max pending). SCTP is tuned globally once (4 MiB windows,
20 ms delayed SACK, initial congestion window 10, 100 ms minimum RTO), which lifts throughput on
high-latency relay links by an order of magnitude.

### Messages

`MeshMsg` is a SCALE enum with stable tags: `Hello`(0), `BufferMap`(1), `Want`(2), `Have`(3),
`SegmentData`(4), `Cancel`(5), `Choke`(6), `Unchoke`(7), `Ping`(8), `Pong`(9), `PeerGossip`(10),
`Subscribe`(11), `Unsubscribe`(12), `EdgeAnnounce`(13), `PresenceGossip`(14), plus `WantInit` and
`InitData`. `Subscribe`/`Unsubscribe` drive a push subscription: the broadcaster and relays push new
chunks and edge announcements to subscribers rather than waiting to be asked. `PresenceGossip` spreads
full presence records as dial hints; a forged one only costs a wasted dial.

### Piece picker

Over the window `[play_seq, play_seq + W]` (W = 64), three zones:

- **Panic** (within ~3 s of a chunk's deadline): earliest-deadline-first, hedged across the top two
  holders; if no holder can meet the deadline, escalate to a relay, then to the Bulletin backup.
- **Mid**: score `U = 1.0·urgency + 0.3·rarity`, sampled in proportion to `(U / U_max)^4`.
- **Prefetch** (beyond two-thirds of the window): rarest-first, no fallback.

Peers are ranked by expected delivery time,
`(pending_bytes + chunk_bytes)·8 / throughput + rtt`, divided by a reputation score in `[0.1, 1.0]`.
Throughput and RTT are exponential moving averages from real deliveries and Ping/Pong.

### Budgets, fairness, reputation

Each node has an upload budget (a byte token bucket; 0 means unmetered) and serves at a paced rate.
It keeps a small set of unchoked upload slots plus one rotating optimistic slot, re-evaluated every
5 seconds; viewers rank their peers by reciprocated throughput (tit-for-tat), while relays and
broadcasters rank by lowest RTT and never choke. Misbehavior costs reputation (a hash failure halves
it; timeouts and protocol abuse cost less and can't ban on their own), verified deliveries slowly
heal it, and a peer below the floor is choked, closed, and banned for 10 minutes. Reassembly is keyed
per `(sender, seq)` so a bad peer can't poison an honest peer's in-progress chunk.

## Discovery, dialing, and origin-shield

A viewer merges chain presence with peers learned through mesh gossip, screens out banned peers,
prefers relay-capable peers, and dials to maintain a target number of links with exponential backoff.
Signaling is the sealed SDP/ICE exchange above.

**Origin-shield** lets a broadcaster serve only through relays so viewers never learn its address. The
gate admits an incoming connection only if the sender's **chain-verified statement proof signer** (the
account the chain itself checked, not any self-asserted field) is in the set of relay accounts the
broadcaster recruited. That set is populated only from chain-verified presence reads, never from
gossip, which is what closes the "anyone can claim to be a relay" hole. With no allowlist configured,
it falls back to the legacy self-asserted flag; the [Security](security.md) page covers why the
hardened path matters.

## The fast path

An optional real-time media side-path (WebRTC media, i.e. RTP, not the data channel) for invited
viewers. The broadcaster runs one sendonly H.264 egress per fast viewer and fans the same access units
the mesh sees to all of them; signaling rides a separate fast-signaling topic. It's unverified (a
media stream, not content-addressed), publisher-direct (no mesh, no relays), invite-gated, and capped
(5 concurrent fast viewers). The verified mesh stays warm as the automatic fallback.

## Constants reference

Concrete values worth pinning. Timings are wall-clock; sizes are bytes.

| Quantity | Value |
|----------|-------|
| RTMP segment duration | ~1 s |
| WHIP / camera CMAF part | ~250 ms |
| Segment window (W) | 64 |
| Scheduler tick | 100 ms |
| Publisher upload budget | 80 Mbps |
| Data-channel chunk | 16 KiB |
| Per-peer paced serve | 256 KiB/tick (~20 Mbps) |
| Bulk in-flight / max pending | 64 KiB / 3 MiB |
| SCTP window / delayed SACK / ICW | 4 MiB / 20 ms / 10 |
| Upload slots (+ optimistic) | 4 (+1) |
| Choke re-evaluation | 5 s |
| Edge re-announce | 1 s |
| Presence gossip | 3 s |
| Signal/edge poll | 800 ms active / 4 s idle / 30 s dormant |
| Presence refresh / TTL | 10 s / 30 s |
| Dial timeout / backoff | 90 s / 2–60 s |
| Reputation floor / heal | 0.05 / +0.02 per verified delivery |
| Ban duration | 600 s |
| Relay: max streams / total budget / per-stream floor | 8 / 50 Mbps / 4 Mbps |
| Relay: idle / stall / dormant evict | 600 s / 60 s / 60 s |
| Relay: per-publisher cap | 2 streams |
| Volunteer announce / TTL | 120 s / 240 s |
| Recruit poll / freshness window | 30 s / ±600 s |
| Fast-path viewer cap | 5 |
| STUN defaults | Google (19302), Cloudflare (3478) |
| Verified-mesh latency (target) | a few seconds |
| Fast-path latency (target) | under 2 s |

Some values are configurable through environment variables (`UNSTATION_NODE_BUDGET_MBPS`,
`UNSTATION_NODE_MAX_STREAMS`, `UNSTATION_STUN`, `UNSTATION_TURN`, and others); see
[Run a relay](run-a-relay.md) and the crate READMEs.
