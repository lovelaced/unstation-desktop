//! Real-transport impairment WITHOUT sudo — a reusable userspace shaping harness for the
//! production `LibDcTransport`, the in-CI cousin of the `dnctl`/dummynet loopback that
//! found the join-drop.
//!
//! Two real transport reactors complete an offer/answer + trickle-ICE handshake, but
//! their ICE candidates are rewritten in the (test-controlled) signaling pump to point
//! at a userspace UDP relay instead of at each other, so ALL media flows through the
//! relay. The relay shapes it: base one-way `delay` (RTT), a serialized `bandwidth`
//! limit, a finite queue (drop on overrun), and random loss — learning each peer's real
//! address from its first packet and swap-forwarding. This exercises the real
//! SCTP/ICE/DTLS stack under RTT+bandwidth, no root required.
//!
//! What it proves here: ICE completes through the userspace proxy, and a whole-segment
//! bulk burst (3.2 MiB, the joining-viewer serve pattern) delivers over the shaped link
//! without the association stalling or dropping — a positive resilience guard for the
//! chunk-pacing path over a real transport.
//!
//! CAVEAT (important): this does NOT reproduce the exact `SCTP disconnected` reset that
//! the sudo `dnctl` shaping did. usrsctp's own congestion control paces the wire, so a
//! *clean* userspace relay never floods hard enough to starve ICE — the on-device reset
//! needed the OS socket path + WiFi MAC behavior that dnctl shapes directly. The relay is
//! the reusable foundation; the definitive drop-repro remains the dnctl loopback
//! (scratchpad `emu-up.sh`), documented in the streaming-assurance memory.
//!
//! Needs loopback UDP for ICE; `--ignored`, run on a networked host.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::time::Instant;
use transport_libdc::{LibDcTransport, SignalOut};
use unstation_core::transport::{Channel, EngineEvent};
use unstation_core::types::PeerId;

/// A userspace UDP relay that shapes the traffic between exactly two peers. Both peers'
/// ICE candidates are rewritten to `addr`, so each peer thinks the *other* lives at the
/// relay; the relay learns the two real addresses and swap-forwards, applying a
/// serialized-link `delay` + `bandwidth` (the queue a burst overruns).
struct ImpairedRelay {
    addr: SocketAddr,
}

impl ImpairedRelay {
    async fn spawn(
        delay: Duration,
        bandwidth_bps: u64,
        max_queue: Duration,
        loss: f64,
        seed: u64,
    ) -> ImpairedRelay {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind relay"));
        let addr = sock.local_addr().unwrap();
        // Forward queue: (payload, dst). A single serialized forwarder models the shared
        // wire — a bulk burst enqueued ahead of an ICE keepalive delays it, which is the
        // whole point.
        let (fwd_tx, mut fwd_rx) = unbounded_channel::<(Vec<u8>, SocketAddr)>();

        // Receive loop: learn the (≤2) endpoints and enqueue each packet to the OTHER.
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let mut eps: Vec<SocketAddr> = Vec::new();
                let mut buf = vec![0u8; 65536];
                loop {
                    let Ok((n, src)) = sock.recv_from(&mut buf).await else { break };
                    if !eps.contains(&src) && eps.len() < 2 {
                        eps.push(src);
                    }
                    if let Some(dst) = eps.iter().find(|&&e| e != src).copied() {
                        let _ = fwd_tx.send((buf[..n].to_vec(), dst));
                    }
                }
            });
        }
        // Forwarder: serialized-link shaping. `wire_free` is when the wire finishes the
        // previous transmission; a packet transmits for `tx` then propagates for `delay`.
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let mut rng = StdRng::seed_from_u64(seed);
                let mut wire_free = Instant::now();
                while let Some((payload, dst)) = fwd_rx.recv().await {
                    if loss > 0.0 && rng.gen::<f64>() < loss {
                        continue;
                    }
                    let tx = if bandwidth_bps > 0 {
                        Duration::from_micros(payload.len() as u64 * 8 * 1_000_000 / bandwidth_bps)
                    } else {
                        Duration::ZERO
                    };
                    let now = Instant::now();
                    // Finite queue: if the wire backlog already exceeds `max_queue`, this
                    // packet would overrun the buffer — DROP it (this is what starves ICE
                    // keepalives behind a bulk burst, exactly like a real hop's queue).
                    if wire_free > now + max_queue {
                        continue;
                    }
                    let tx_start = now.max(wire_free);
                    wire_free = tx_start + tx;
                    let arrive = tx_start + tx + delay;
                    let sock = sock.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(arrive).await;
                        let _ = sock.send_to(&payload, dst).await;
                    });
                }
            });
        }
        ImpairedRelay { addr }
    }
}

/// Rewrite the host-candidate address in an ICE candidate JSON so ICE routes through the
/// relay. Loopback host candidates carry `127.0.0.1 <port>`; replace the port.
fn route_through_relay(cand: &[u8], relay: SocketAddr) -> Vec<u8> {
    let s = String::from_utf8_lossy(cand);
    let needle = "127.0.0.1 ";
    if let Some(idx) = s.find(needle) {
        let after = idx + needle.len();
        let digits = s[after..].chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            return format!("{}{}{}", &s[..after], relay.port(), &s[after + digits..]).into_bytes();
        }
    }
    cand.to_vec()
}

/// Pump one transport's signaling to the other, rewriting candidates through the relay.
/// `is_offerer` marks the side whose first description is the offer (→ `accept`).
fn pump(
    mut rx: UnboundedReceiver<SignalOut>,
    to: LibDcTransport,
    from_id: PeerId,
    is_offerer: bool,
    relay: SocketAddr,
) {
    tokio::spawn(async move {
        let mut accepted = false;
        while let Some(sig) = rx.recv().await {
            match sig {
                SignalOut::LocalDescription { sdp, .. } => {
                    if is_offerer && !accepted {
                        to.accept(from_id, sdp);
                        accepted = true;
                    } else {
                        to.remote_description(from_id, sdp);
                    }
                }
                SignalOut::LocalCandidate { cand, .. } => {
                    to.remote_candidate(from_id, route_through_relay(&cand, relay));
                }
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
async fn bulk_burst_over_a_shaped_relay_does_not_drop_the_link() {
    let a_id = PeerId::from_u64(1);
    let b_id = PeerId::from_u64(2);

    // ~80 ms RTT, 10 Mbit/s serialized, a 100 ms queue — a realistically constrained hop.
    let relay =
        ImpairedRelay::spawn(Duration::from_millis(40), 10_000_000, Duration::from_millis(100), 0.0, 7)
            .await;

    let (a_inbox_tx, mut a_inbox_rx) = unbounded_channel::<EngineEvent>();
    let (b_inbox_tx, mut b_inbox_rx) = unbounded_channel::<EngineEvent>();
    let (a_sig_tx, a_sig_rx) = unbounded_channel::<SignalOut>();
    let (b_sig_tx, b_sig_rx) = unbounded_channel::<SignalOut>();

    let a = LibDcTransport::new(vec![], a_inbox_tx, a_sig_tx).expect("spawn A");
    let b = LibDcTransport::new(vec![], b_inbox_tx, b_sig_tx).expect("spawn B");

    pump(a_sig_rx, b.clone(), a_id, true, relay.addr); // A's first description is the offer
    pump(b_sig_rx, a.clone(), b_id, false, relay.addr);
    a.dial(b_id);

    // Come up through the relay (slower than direct loopback — allow 30 s).
    let link = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match a_inbox_rx.recv().await {
                Some(EngineEvent::PeerConnected { link, .. }) => return link,
                Some(_) => continue,
                None => panic!("A inbox closed before connect"),
            }
        }
    })
    .await
    .expect("handshake did not complete through the relay in 30 s");

    // Blast a whole catch-up window: 200 × 16 KiB = 3.2 MiB, back to back, on the bulk
    // channel — exactly what a joining viewer's serve looks like. Chunk pacing must meter
    // this so ICE keepalives still get through; without it the association resets.
    const CHUNKS: usize = 200;
    let chunk = vec![0xABu8; 16 * 1024];
    for _ in 0..CHUNKS {
        link.send(Channel::Bulk, chunk.clone());
    }

    // Over the next 40 s: the connection must NOT drop, and most of the (unreliable) bulk
    // chunks should arrive. A drop shows up as `PeerDisconnected` on A's inbox.
    let mut received = 0usize;
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = b_inbox_rx.recv() => match ev {
                Some(EngineEvent::Inbound { channel: Channel::Bulk, .. }) => {
                    received += 1;
                    if received >= CHUNKS { break; }
                }
                Some(_) => {}
                None => break,
            },
            ev = a_inbox_rx.recv() => {
                if let Some(EngineEvent::PeerDisconnected { .. }) = ev {
                    panic!("association dropped under the bulk burst over the shaped relay (received {received}/{CHUNKS})");
                }
            }
        }
    }

    // Unreliable channel (maxRetransmits=0) → some loss under load is fine; the invariant
    // is that the LINK held and the paced burst mostly got through.
    assert!(
        received >= CHUNKS * 3 / 4,
        "link held but only {received}/{CHUNKS} bulk chunks arrived — pacing may be starving throughput",
    );
}
