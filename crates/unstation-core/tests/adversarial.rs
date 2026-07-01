//! Adversarial-peer suite: real `MeshNode`s over the in-memory transport, attacked
//! by hand-driven hostile actors (a forger, a flooder, a buffer-map liar). Proves
//! the A1/A2 hardening end to end: playback survives, memory stays inside its
//! caps, and the attacker is scored down / banned — deterministically, no network.

use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use unstation_core::buffermap::BufferMap;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::protocol::{Caps, MeshMsg};
use unstation_core::signaling::BanList;
use unstation_core::transport::{Channel, EngineEvent, Link};
use unstation_core::transport_mem::wire;
use unstation_core::types::{PeerId, SegmentId, Seq};

#[derive(Default)]
struct Rec {
    got: Mutex<BTreeSet<u64>>,
}
impl MediaSink for Rec {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, _: Bytes) {
        self.got.lock().unwrap().insert(seq);
    }
    fn on_play_head(&self) -> u64 {
        0
    }
}

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Vod,
        role,
        window: 16,
        tick: Duration::from_millis(10),
        seg_ms: 500,
        upload_budget_bps: 50_000_000,
        weights: PickerWeights::default(),
    }
}

/// Segments small enough to travel as a single chunk (keeps hostile actors simple).
const SEG_LEN: usize = 12_000;

fn make_vod(n: usize) -> (Vec<Bytes>, HashMap<Seq, SegmentId>) {
    let segments: Vec<Bytes> = (0..n).map(|i| Bytes::from(vec![i as u8; SEG_LEN])).collect();
    let ids = segments
        .iter()
        .enumerate()
        .map(|(i, b)| (i as Seq, crypto::segment_id(b)))
        .collect();
    (segments, ids)
}

/// A `Hello` advertising `0..n` — how a hostile actor makes the picker consider it.
fn full_hello(me: PeerId, n: usize) -> Vec<u8> {
    let mut bm = BufferMap::new(0);
    for s in 0..n as Seq {
        bm.set(s);
    }
    MeshMsg::Hello {
        peer_id: me.0,
        stream_id: [0u8; 32],
        version: 1,
        caps: Caps { upload_bps: 50_000_000, relay: false },
        base_seq: bm.base(),
        bitfield: bm.to_bytes(),
    }
    .encode()
}

/// Wire an honest publisher (full VOD) to the viewer inbox. Returns its Stop sender.
fn spawn_publisher(
    pubid: PeerId,
    viewid: PeerId,
    view_tx: &UnboundedSender<EngineEvent>,
    segments: Vec<Bytes>,
) -> UnboundedSender<EngineEvent> {
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (link_for_pub, link_for_view) = wire(pubid, ptx.clone(), viewid, view_tx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: link_for_pub }).unwrap();
    view_tx.send(EngineEvent::PeerConnected { peer: pubid, link: link_for_view }).unwrap();
    let publisher = MeshNode::new_publisher(
        pubid,
        cfg(Role::Publisher),
        SEG_LEN as u64,
        Arc::new(NullSink),
        segments,
    );
    tokio::spawn(publisher.run(prx, Duration::from_millis(10), None));
    ptx
}

/// A hand-driven hostile actor: advertises the full VOD, then feeds every inbound
/// message to `react`, which may send crafted frames back over the link.
fn spawn_hostile(
    me: PeerId,
    viewid: PeerId,
    view_tx: &UnboundedSender<EngineEvent>,
    n: usize,
    react: impl Fn(&MeshMsg, &Arc<dyn Link>) + Send + 'static,
) {
    let (mtx, mut mrx): (UnboundedSender<EngineEvent>, UnboundedReceiver<EngineEvent>) =
        mpsc::unbounded_channel();
    let (link_for_m, link_for_view) = wire(me, mtx.clone(), viewid, view_tx.clone());
    view_tx.send(EngineEvent::PeerConnected { peer: me, link: link_for_view }).unwrap();
    link_for_m.send(Channel::Ctrl, full_hello(me, n));
    tokio::spawn(async move {
        while let Some(ev) = mrx.recv().await {
            if let EngineEvent::Inbound { bytes, .. } = ev {
                if let Ok(msg) = MeshMsg::decode(&mut &bytes[..]) {
                    react(&msg, &link_for_m);
                }
            }
        }
    });
}

fn frame_segment(seq: Seq, payload: &[u8]) -> Vec<u8> {
    MeshMsg::SegmentData {
        seq,
        track_id: 0,
        total_len: payload.len() as u32,
        offset: 0,
        bytes: payload.to_vec(),
    }
    .encode()
}

/// A forger that is the viewer's ONLY peer: every segment it serves is garbage,
/// so it is reliably asked, reliably fails verification, and is banned. (The
/// honest publisher is deliberately absent here — with the instant, symmetric
/// mem transport its bytes would race the forger's and non-deterministically
/// win before the hash check, which is exactly what the survival test exercises.)
#[tokio::test]
async fn forger_is_banned() {
    let n = 12usize;
    let (_segments, ids) = make_vod(n);
    let viewid = PeerId::from_u64(2);
    let (view_tx, view_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let forger = PeerId::from_u64(66);
    spawn_hostile(forger, viewid, &view_tx, n, move |msg, link| {
        if let MeshMsg::Want { segment_seqs, .. } = msg {
            for &seq in segment_seqs {
                link.send(Channel::Bulk, frame_segment(seq, &vec![0xEEu8; SEG_LEN]));
            }
        }
    });

    let bans = BanList::new();
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        SEG_LEN as u64,
        Arc::new(NullSink),
        ids,
        (n - 1) as Seq,
    )
    .with_ban_list(bans.clone());
    // No completion target — it can never finish from a pure forger; we stop it once
    // the conviction lands (or the guard fires).
    let handle = tokio::spawn(viewer.run(view_rx, Duration::from_millis(10), None));

    let banned = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if bans.contains(&forger) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(banned.is_ok(), "the forger must be convicted and shared with the session");

    let _ = view_tx.send(EngineEvent::Stop);
    let stats = handle.await.unwrap();
    assert!(stats.hash_failures > 0, "the forger's garbage reached verification");
}

/// Playback must complete from an honest publisher even with a forger in the swarm
/// answering every `Want` with garbage — the forger's bytes are discarded on hash
/// mismatch and the picker keeps making progress from the honest side.
#[tokio::test]
async fn playback_survives_a_forger_in_the_swarm() {
    let n = 12usize;
    let (segments, ids) = make_vod(n);
    let viewid = PeerId::from_u64(2);
    let (view_tx, view_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let ptx = spawn_publisher(PeerId::from_u64(1), viewid, &view_tx, segments);
    let forger = PeerId::from_u64(66);
    spawn_hostile(forger, viewid, &view_tx, n, move |msg, link| {
        if let MeshMsg::Want { segment_seqs, .. } = msg {
            for &seq in segment_seqs {
                link.send(Channel::Bulk, frame_segment(seq, &vec![0xEEu8; SEG_LEN]));
            }
        }
    });

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        SEG_LEN as u64,
        rec.clone(),
        ids,
        (n - 1) as Seq,
    )
    .with_ban_list(BanList::new());

    let stats = tokio::time::timeout(
        Duration::from_secs(20),
        viewer.run(view_rx, Duration::from_millis(10), Some(n)),
    )
    .await
    .expect("playback must complete despite the forger");

    assert_eq!(stats.delivered, n, "every segment delivered from the honest publisher");
    assert_eq!(rec.got.lock().unwrap().len(), n);

    let _ = ptx.send(EngineEvent::Stop);
}

/// A peer that sprays unsolicited `SegmentData` for thousands of seqs. Reassembly
/// memory must stay inside its caps and playback must be unaffected.
#[tokio::test]
async fn unrequested_flood_is_bounded() {
    let n = 10usize;
    let (segments, ids) = make_vod(n);
    let viewid = PeerId::from_u64(2);
    let (view_tx, view_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let ptx = spawn_publisher(PeerId::from_u64(1), viewid, &view_tx, segments);

    // The flooder doesn't wait to be asked: it fires partial first-chunks for two
    // thousand seqs straight into the viewer (some in-window, most far outside).
    let flooder = PeerId::from_u64(77);
    let (ftx, _frx) = mpsc::unbounded_channel::<EngineEvent>();
    let (link_for_f, link_for_view) = wire(flooder, ftx, viewid, view_tx.clone());
    view_tx.send(EngineEvent::PeerConnected { peer: flooder, link: link_for_view }).unwrap();
    link_for_f.send(Channel::Ctrl, full_hello(flooder, n));
    for seq in 0..2_000u64 {
        // total_len claims a large segment; only the first 4 KiB chunk is ever sent.
        link_for_f.send(
            Channel::Bulk,
            MeshMsg::SegmentData {
                seq,
                track_id: 0,
                total_len: 3 * 1024 * 1024,
                offset: 0,
                bytes: vec![0xAB; 4096],
            }
            .encode(),
        );
    }

    let bans = BanList::new();
    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        SEG_LEN as u64,
        rec.clone(),
        ids,
        (n - 1) as Seq,
    )
    .with_ban_list(bans.clone());

    let stats = tokio::time::timeout(
        Duration::from_secs(20),
        viewer.run(view_rx, Duration::from_millis(10), Some(n)),
    )
    .await
    .expect("playback must complete under the flood");

    assert_eq!(stats.delivered, n);
    assert_eq!(stats.hash_failures, 0, "garbage never reached verification");
    assert!(stats.reasm_entries <= 64, "entry cap held: {}", stats.reasm_entries);
    assert!(
        stats.reasm_bytes <= 32 * 1024 * 1024,
        "byte cap held: {}",
        stats.reasm_bytes
    );
    assert!(bans.contains(&flooder), "the out-of-window spray convicted the flooder");

    let _ = ptx.send(EngineEvent::Stop);
}

/// A peer that advertises every segment but never serves a single byte. The
/// timeout penalty demotes it and the picker reroutes to the honest publisher.
#[tokio::test]
async fn buffermap_liar_is_rerouted_around() {
    let n = 12usize;
    let (segments, ids) = make_vod(n);
    let viewid = PeerId::from_u64(2);
    let (view_tx, view_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let ptx = spawn_publisher(PeerId::from_u64(1), viewid, &view_tx, segments);
    let liar = PeerId::from_u64(88);
    spawn_hostile(liar, viewid, &view_tx, n, |_msg, _link| {
        // Advertises everything (full_hello above), answers nothing.
    });

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        SEG_LEN as u64,
        rec.clone(),
        ids,
        (n - 1) as Seq,
    );

    let stats = tokio::time::timeout(
        Duration::from_secs(30),
        viewer.run(view_rx, Duration::from_millis(10), Some(n)),
    )
    .await
    .expect("timeout penalties must reroute every request to the honest publisher");

    assert_eq!(stats.delivered, n, "all segments delivered despite the liar");
    assert_eq!(stats.hash_failures, 0);
    assert_eq!(
        stats.peer_bytes as usize,
        n * SEG_LEN,
        "every byte ultimately came from the honest side"
    );
    assert_eq!(stats.pending_entries, 0, "no request left dangling");

    let _ = ptx.send(EngineEvent::Stop);
}
