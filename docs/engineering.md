# Engineering Q&A

For people who would read the source. This covers the mesh layer and the SCALE wire format: the
decisions, what they cost, and what they buy. The [Protocol](protocol.md) page is the reference (what
the bytes are); this page is the rationale (why they are that way). The [Security](security.md) page
is the threat model.

## The engine

### Why is the core engine pure, with no IO?

The mesh engine (`unstation-core`) takes no clock, no sockets, and no chain client. Time is an injected
tick, the transport and signaling are traits, and the scheduler is a single pure function that maps
`(state, now) -> actions`. Production wires those traits to real WebRTC and a real clock; the
simulator wires them to a virtual clock and an in-memory network. Both run the identical decision
code.

That buys two things. The first is a deterministic, seeded simulator that runs hundreds of virtual
peers with per-link loss, latency, jitter, and churn, and replays byte-for-byte from a seed, so an
adversarial-peer or churn regression is reproducible instead of a heisenbug. The second is that the
hard part of the system (piece selection, scoring, choking, reassembly) is testable without a
network at all. The cost is discipline: no engine code may reach for wall-clock time or IO, and every
effect has to be expressed as an action the host performs. For a live mesh, where the failure modes
are timing races and hostile peers, that trade is worth it.

### Why split into so many crates?

The engine has no dependency on the chain SDK, and the chain-facing crates do. Keeping them separate
means `cargo test --workspace` builds and runs the whole deterministic suite in seconds without the
SDK, which is the loop you want tight. The chain crates, the Tauri shell, and the relay live outside
that workspace and are built from their own manifests. The coverage gate rides the engine workspace
for the same reason: the part that must not regress is the pure logic, and it can be measured without
external tools (with one exception, the media segmenter, which needs ffmpeg).

## The mesh: fetching video

### Why deadline-aware piece picking instead of rarest-first?

BitTorrent optimizes for eventually downloading a whole file, so rarest-first is right: it maximizes
the number of complete copies in the swarm. Live video has a deadline. A segment that arrives after
its play time is worthless, worse than worthless because you spent upload budget fetching it. So the
picker is organized around "will this chunk arrive before I need to play it," and rarity is a
secondary term used only where there is slack.

### Walk me through the three picker zones.

The picker looks at the window `[play_seq, play_seq + W]` and splits it by urgency:

- **Panic** (a chunk within about 3 seconds of its play deadline): earliest-deadline-first, and it
  hedges by requesting from the two best holders at once. If no holder can plausibly meet the
  deadline, it escalates to a relay, then to the durable backup on Bulletin. This is where a stall is
  won or lost, so it spends redundancy to avoid one.
- **Mid** (has slack): score each candidate `U = 1.0 * urgency + 0.3 * rarity` and sample in
  proportion to `(U / U_max)^4`. The exponent concentrates picks on the urgent-and-rare chunks
  without going fully greedy.
- **Prefetch** (beyond two-thirds of the window): rarest-first with no fallback. Out here there is
  time, so the goal flips to spreading copies through the swarm, which is exactly what rarest-first
  does.

### Why weighted-random sampling in the mid zone instead of strict priority?

Strict priority makes every peer independently compute the same "most important chunk" and converge
on it, which wastes the swarm's aggregate bandwidth re-fetching a few chunks while others go
unrequested. Sampling proportional to a sharpened score keeps each peer mostly on the right chunks but
decorrelates their choices, so the load spreads. It is the same reason randomized backoff beats a
fixed one. The `^4` is the knob: high enough that urgent chunks still dominate, low enough to avoid a
thundering herd.

### How are peers ranked for a given chunk?

By expected delivery time, not by raw throughput. For a candidate peer:

```
expected_ms = (pending_bytes + segment_bytes) * 8 / throughput_bps * 1000 + rtt_ms
score       = expected_ms / reputation.clamp(0.1, 1.0)
```

`throughput_bps` and `rtt_ms` are exponential moving averages from actual deliveries and Ping/Pong,
so the estimate tracks reality and decays stale samples. `pending_bytes` accounts for what you have
already asked that peer for, so you do not pile a deadline-critical chunk behind a queue. Dividing by
reputation biases away from peers that have misbehaved without hard-banning them for one bad sample.
The whole thing is an estimate of "when would this chunk actually land," which is the quantity the
deadline logic cares about.

### Why tit-for-tat choking for viewers but not for seeds and publishers?

A viewer has scarce upload (it is also trying to receive), so it rations: a small set of unchoked
slots plus one rotating optimistic slot, re-evaluated every few seconds, with peers ranked by
reciprocated throughput. That is incentive-compatible; a peer that feeds you gets fed. A seed or a
publisher exists to serve. Choking there would defeat the point, so they never choke and rank their
service by lowest RTT instead, spending their budget where it lands fastest. The optimistic slot
matters for both: it is how a newly-arrived peer with no reciprocity history ever gets a first chunk
to bootstrap from.

## The transport

### Why two WebRTC data channels?

One reliable-ordered channel (`ctrl`) and one unreliable-unordered channel (`bulk`). Control messages
(buffer maps, wants, edge announcements, pings) must arrive and mostly must arrive in order, so they
take the reliable channel. Video chunks must not head-of-line-block each other: if chunk 41's packet
is lost, you do not want chunk 42 stuck behind a retransmit, because 42 might still make its deadline
and 41 might already be moot. So bulk is unordered with no retransmits, and loss is handled at the
application layer by re-requesting the chunk from whoever is now the best holder. Putting both on one
reliable-ordered channel would reintroduce exactly the head-of-line blocking the design avoids.

### Why chunk at 16 KiB and hand-frame, instead of leaning on SCTP?

Framing chunks yourself gives explicit control over pacing, partial-delivery accounting, and the size
of the unit that can be lost and re-requested. A 16 KiB application frame is a clean unit to pace and
to reason about: bulk sending targets a bounded amount in flight and a bounded pending backlog, so a
fast peer cannot bufferbloat a slow link, and the reassembler tracks progress per chunk. Delegating
all of that to SCTP's own fragmentation would hand away the pacing and the loss-unit control that the
deadline logic depends on.

### What is the SCTP tuning about?

libdatachannel's defaults are tuned for low-latency local links. A relay is often a high-RTT path
(a viewer on another continent from the volunteer carrying the stream), and on a 200 ms link the
stock send/receive windows and delayed-SACK cap throughput at around 17 Mbit/s, far below the link's
real capacity. Widening the windows to 4 MiB, tightening delayed-SACK to 20 ms, raising the initial
congestion window to 10, and setting a 100 ms minimum RTO lifts that same link to roughly 167 Mbit/s.
It is applied once, process-wide. Without it, relays over long paths are throughput-starved for no
reason other than defaults.

### Why key reassembly per (sender, seq)?

A chunk arrives as multiple 16 KiB frames that get reassembled before the hash check. If reassembly
were keyed by `seq` alone, a malicious peer could send frames for a chunk you are also fetching from
a legitimate peer and corrupt the in-progress buffer, causing that chunk to fail its hash and the
legitimate peer to be penalized. Keying by `(sender, seq)` isolates each peer's contribution: a bad
peer can only ever corrupt its own reassembly slot, which then fails verification and costs that
peer, not a legitimate one. It is a small structural choice that closes a real cross-peer poisoning
vector.

### How is memory bounded against a huge or hostile stream?

Everything that grows is pinned to the live window. A seed pins its play cursor to `head - window/2`
and prunes below the window each tick, so a multi-hour stream holds a bounded cache rather than the
whole history. Bytes that arrive ahead of their verifying id are held in a capped pending-verify
buffer and dropped if the cap is exceeded. Reassembly has a bounded number of in-flight chunks and a
byte ceiling. The buffer-map bitfield slides its base forward with playback so it cannot grow without
bound, which also bounds the size of the map advertised to every peer every few hundred milliseconds.
The wire decoders treat a hostile length or a near-max base as input to reject, not to allocate
against (a fuzz-found overflow in the buffer map's highest-held computation is fixed with checked
arithmetic).

## Discovery and signaling

### Why a Polkadot statement store instead of a DHT or a signaling server?

A signaling server is the single point the whole design exists to remove. A DHT is operator-free but
brings its own problems: Sybil-cheap routing, no built-in spam control, and eclipse attacks. The
statement store is a permissionless, personhood-gated bulletin board: anyone can post after proving
they are a real person, which is what keeps bots from flooding discovery, and there is no operator to
lean on. The costs are real and the design works around them: statements are last-write-wins per
(account, channel), they linger on the order of an hour past their intended lifetime, and the topics
a client reads are visible metadata. The first two drive the wire format (below); the third is covered
in [Security](security.md).

### Why the accumulate-and-rewrite signaling bundle?

Last-write-wins per (account, channel) means only one statement survives on a given topic per account.
An offer, its answer, and trickled ICE candidates posted as separate statements would evict each
other, and the reader would only ever see the last one. So a sender does not post messages
individually. It keeps the full set of sealed envelopes it has produced for a peer and rewrites the
entire bundle (a SCALE `Vec<Vec<u8>>`) on each update, with a strictly increasing priority so the
newest, largest bundle always wins. The reader unpacks the set and deduplicates. It is a workaround
for a specific store semantic, and it is cheap because signaling bundles are small and short-lived.

### Push or poll for the live edge?

Both, with push as the fast path and poll as reconciliation. The store delivers new statements to a
subscription, so a viewer learns of a new edge announcement or an inbound offer the moment it lands,
rather than waiting out a poll interval. Polling stays underneath as a slower reconciliation loop that
catches anything a dropped subscription missed. Inside the mesh, the live edge also propagates by
signed gossip over the data channels, and the publisher and relays proactively push new chunks to
subscribers rather than waiting to be asked, which is what keeps the glass-to-glass latency down.

## Wire format: SCALE

### Why SCALE?

The same process talks to a Polkadot chain, so using the chain's own codec everywhere means one
encoding to reason about, one set of derive macros, and no impedance mismatch between the mesh wire
and the statement payloads. SCALE is compact, canonical (a given value has one encoding, which
matters when you content-address or sign it), and derive-driven, so wire types are ordinary Rust
structs and enums with `#[derive(Encode, Decode)]`. The tradeoffs are that it is not self-describing
(no field names or tags on the wire, so both ends must agree on the type), it has less tooling outside
the Substrate ecosystem than protobuf, and enum tag stability is your responsibility rather than the
format's. For a system already living in that ecosystem, the alignment outweighs the tooling gap.

### How do you keep the wire stable and forward-compatible?

The mesh message enum uses stable, explicit discriminants and is append-only: new message kinds get
new tags at the end, existing tags never move. Records that cross the chain (presence, volunteer,
recruitment) carry an explicit version field so a reader can reject or adapt to an unknown major
version rather than misparse it. Because SCALE is not self-describing, this is the discipline that
replaces a schema registry: additive changes are safe, and anything else is a version bump the
reader checks.

### Why content-address chunks with BLAKE2b-256?

Content addressing does two jobs at once: it deduplicates (the same chunk from any peer has the same
name) and it makes tampering self-evident (the name is the hash, so a wrong byte is a wrong name).
BLAKE2b-256 is the hash the Polkadot ecosystem uses, so the same primitive addresses chunks, derives
topics, and keys the store, with no second hash function in the trust path. The id is not metadata
attached to a chunk; the id is the integrity check.

### Why domain-separated signing contexts?

Every sr25519 signature is made under a context string: manifests under one, live-edge announcements
under another, recruitment messages under a third. Without domain separation, a signature produced
for one purpose could be replayed as a valid signature for another (an edge announcement re-presented
as a manifest, say). Separate contexts make each signature valid only for its intended message type.
It is standard signing hygiene, and it is cheap, so there is no reason not to.

## Trust and Sybil resistance

### What is cryptographic here, and what is economic?

Content integrity is cryptographic and absolute: a chunk is named by its hash and the edge that names
it is signed by a personhood key, so no peer, relay, or Sybil can feed you altered video without the
verification failing. Bandwidth contribution is economic and best-effort: a peer can refuse to serve,
serve slowly, or vanish. The reputation and choking system handles that class, scoring down and
eventually banning peers that fail to deliver or that misbehave on the wire. The clean line between
the two is what makes it safe to accept data from untrusted strangers: the worst a Sybil can do is
waste your dials and your time, never corrupt what you play.

### Why are the reputation multipliers set the way they are?

Not all misbehavior is equal. A hash failure means a peer served bytes that did not match the signed
id, which is either malice or corruption, so it halves reputation immediately. A timeout is softer
(networks are flaky, and a legitimate peer on a bad link should not be exiled for one slow response),
so it costs less and is floored so that timeouts alone can never drive a peer to a ban. Protocol abuse
sits in between. Verified deliveries heal reputation slowly, so a peer earns its way back. A ban is
time-boxed, not permanent, because a `PeerId` is cheap and a permanent ban would mostly punish churn.

## Latency and what the design enables

### Why is verified delivery a few seconds, and where is the floor?

The floor is the sum of the parts: segment duration (about a second for the standard path), the
window you buffer to absorb jitter and re-requests, the hash verification, and the mesh hops a chunk
takes to reach you. None of those is large alone; together they land verified playback in the
few-seconds range, which is right for most broadcasts. The fast path exists for cases that need less:
it sends video as a direct real-time media stream to invited viewers and skips per-chunk
verification, trading the integrity guarantee for sub-two-second latency, with the verified mesh still
running underneath as the fallback. There is no way to have both the full per-chunk verification and
sub-second latency at once, so the fast path is an explicit, opt-in, invite-gated trade.

### What does the mesh enable that a server cannot, and what does it cost?

It enables three things a server tier cannot: capacity that grows with the audience instead of
against it, because every viewer contributes upload; no origin to take down or pressure; and no
per-viewer egress bill, because there is no origin paying for it. The costs are equally real: a
latency floor a CDN can beat, connectivity complexity (NAT traversal, the relay tier that stands in
for TURN), and no central authority to enforce quality, so the system has to be robust to peers that
are slow, hostile, or gone. The whole engineering effort is spending that complexity to buy the three
properties.

## Determinism and testing

### How do you test a mesh deterministically?

The pure engine plus a virtual clock is the whole trick. The simulator drives many nodes on a seeded
virtual clock over an in-memory network with configurable per-link RTT, bandwidth, loss, jitter, and
churn, and because the decision function is pure and the clock is injected, a run is fully replayable
from its seed. That turns the usual mesh nightmares (a race that only shows up under 3% loss with a
peer leaving mid-fetch) into deterministic, reproducible tests. Adversarial-peer, churn, and
loss-matrix suites run this way and gate CI. A live mesh has too many timing-dependent failure modes
to test any other way.

### And the wire format?

The decoders parse attacker-controlled bytes (buffer maps, presence records, protocol messages), so
they are fuzzed against hostile input with the requirement that no input, however malformed, may
panic, over-allocate, or corrupt state. That is not hypothetical: fuzzing found a buffer-map operation
that overflowed on a crafted near-maximum base sequence, which a malicious peer could have used to
crash a node. The fix (checked arithmetic) and a regression for the exact input are in the tree, and
the target keeps running so the next such bug surfaces the same way.
