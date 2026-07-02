//! The opt-in WebRTC media fast tier (W3), app-side glue.
//!
//! Two halves that never both run on one machine:
//!
//! * **Publisher** ([`FastTier`], desktop only — needs the media-enabled libdatachannel):
//!   holds a [`whip_ingest::MediaEgress`] per fast-tier viewer, each on its own thread, and
//!   fans the SAME access units the mesh muxer sees onto every one. Bounded by a concurrent-
//!   viewer cap (the publisher's uplink serves the handful directly; the crowd is the mesh's).
//!
//! * **Viewer** ([`spawn_answer_reader`], all builds — the browser does the WebRTC): relays
//!   the browser's SDP offer to the publisher over the fast-tier signaling topic and pumps the
//!   answer back to the webview. No libdatachannel here.
//!
//! Signaling rides [`unstation_chain::ChainSignaling`]'s fast-tier topic (non-trickle: one
//! offer, one answer — the gathered SDP carries the candidates). The verified mesh tier is
//! untouched and stays warm as the automatic fallback if this path never connects.

use unstation_core::signaling::SignalMsg;
use unstation_core::types::PeerId;

/// A synthetic offer id on answers/closes. The fast tier is one-offer-per-viewer, so the
/// viewer applies the first answer addressed to it regardless — this only aids logging.
pub const FAST_OFFER_ID: &str = "fast";

/// SDP is carried as UTF-8 bytes inside [`SignalMsg`]'s `sdp: Vec<u8>`.
fn sdp_bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}
fn sdp_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---- viewer side (all builds) --------------------------------------------------------------

/// Send the browser's `offer_sdp` to `publisher` on the fast-tier topic, then watch for the
/// publisher's answer (or a close) and forward it to the webview as a `fast-answer` /
/// `fast-closed` event. Returns the reader task so the caller can abort it on stop.
///
/// Non-trickle: the offer already carries its gathered ICE candidates, and so will the
/// answer — no per-candidate relay. Deduped by content; the reader stops after the first
/// answer (the browser only needs one).
pub fn spawn_answer_reader(
    app: tauri::AppHandle,
    signaling: unstation_chain::ChainSignaling,
    my_peer: PeerId,
    publisher: PeerId,
    offer_sdp: String,
) -> tokio::task::JoinHandle<()> {
    use tauri::Emitter;
    tokio::spawn(async move {
        // Push wakeups for fast-tier signals addressed to us; also poll as a backstop.
        let mut push = signaling.subscribe_fast_signals_push(my_peer);
        if let Err(e) = signaling
            .publish_fast_signal(my_peer, publisher, SignalMsg::Offer { sdp: sdp_bytes(&offer_sdp) })
            .await
        {
            log::warn!("[fast] offer publish failed: {e:?}");
            let _ = app.emit("fast-closed", ());
            return;
        }
        log::info!("[fast] offer sent to publisher; awaiting answer");
        loop {
            // Read whatever's addressed to us; act on the publisher's answer/close.
            if let Ok(sigs) = signaling.read_fast_signals(my_peer).await {
                for (from, msg) in sigs {
                    if from != publisher {
                        continue;
                    }
                    match msg {
                        SignalMsg::Answer { sdp, .. } => {
                            log::info!("[fast] answer received; handing to the webview");
                            let _ = app.emit("fast-answer", sdp_str(&sdp));
                            return;
                        }
                        SignalMsg::Closed { .. } => {
                            log::info!("[fast] publisher declined (cap/again) — falling back to mesh");
                            let _ = app.emit("fast-closed", ());
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // Wait for a push wakeup or re-poll after a short delay (TTL-safe re-read).
            tokio::select! {
                _ = push.recv() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(700)) => {}
            }
        }
    })
}

/// Tell the publisher this viewer is leaving the fast tier (best-effort), so it can free the
/// slot for someone else. Fire-and-forget.
pub async fn send_fast_close(
    signaling: &unstation_chain::ChainSignaling,
    my_peer: PeerId,
    publisher: PeerId,
) {
    let _ = signaling
        .publish_fast_signal(my_peer, publisher, SignalMsg::Closed { offer_id: FAST_OFFER_ID.into() })
        .await;
}

// ---- publisher side (desktop; media-enabled libdatachannel) --------------------------------

#[cfg(all(feature = "publish", not(target_os = "android")))]
pub use publisher::FastTier;

#[cfg(all(feature = "publish", not(target_os = "android")))]
mod publisher {
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// One video frame to fan out to fast-tier viewers: an Annex-B access unit + its 90kHz
    /// RTP timestamp. Reference-counted so the broadcast clones a pointer, not the bytes.
    enum FastFrame {
        Au(Arc<Vec<u8>>, u32),
    }

    /// Publisher-side fast-tier manager: a sendonly WebRTC media connection per viewer, each
    /// owned by its own thread (libdatachannel handles never leave the thread that made them),
    /// fed the publisher's access units. Bounded by `cap` concurrent viewers.
    pub struct FastTier {
        viewers: Mutex<HashMap<[u8; 32], Sender<FastFrame>>>,
        cap: usize,
        stun: Vec<String>,
    }

    impl FastTier {
        pub fn new(cap: usize, stun: Vec<String>) -> Arc<Self> {
            Arc::new(Self { viewers: Mutex::new(HashMap::new()), cap, stun })
        }

        /// Live fast-tier viewer count (for the publisher dashboard / cap checks).
        #[allow(dead_code)] // surfaced on the publisher dashboard in a follow-up
        pub fn viewer_count(&self) -> usize {
            self.viewers.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        /// Fan one access unit out to every fast-tier viewer. Dead connections (their thread
        /// exited, dropping the receiver) are pruned here — a send that errors removes the
        /// viewer. Called on the ingest AU path, so it must stay cheap: one alloc + Arc clones.
        pub fn broadcast(&self, au: &[u8], ts_90k: u32) {
            let mut v = self.viewers.lock().unwrap_or_else(|e| e.into_inner());
            if v.is_empty() {
                return;
            }
            let frame = Arc::new(au.to_vec());
            v.retain(|_, tx| tx.send(FastFrame::Au(frame.clone(), ts_90k)).is_ok());
        }

        /// Remove a viewer (its thread has exited or it asked to leave).
        pub fn drop_viewer(&self, viewer: &[u8; 32]) {
            self.viewers.lock().unwrap_or_else(|e| e.into_inner()).remove(viewer);
        }

        /// Answer a viewer's SDP offer: bring up a sendonly egress on its own thread and return
        /// the answer SDP to relay back. `None` if the cap is reached (viewer stays on the mesh)
        /// or negotiation fails. A re-offer from a viewer we already serve replaces the old one.
        pub async fn accept_offer(self: &Arc<Self>, viewer: [u8; 32], offer_sdp: String) -> Option<String> {
            {
                let v = self.viewers.lock().unwrap_or_else(|e| e.into_inner());
                if !v.contains_key(&viewer) && v.len() >= self.cap {
                    log::info!("[fast] at capacity ({}); viewer stays on the mesh", self.cap);
                    return None;
                }
            }
            let (frame_tx, frame_rx) = channel::<FastFrame>();
            let (ans_tx, ans_rx) = tokio::sync::oneshot::channel::<Option<String>>();
            let stun = self.stun.clone();
            // The egress + its libdatachannel handles live entirely on this thread. The thread
            // never touches the viewer map: when it exits, its `frame_rx` drops, so the next
            // `broadcast` prunes the stale sender. (It must not `drop_viewer` itself — a re-offer
            // may have already replaced this viewer's sender, and the exiting old thread would
            // otherwise evict the live new one.)
            std::thread::Builder::new()
                .name("fast-egress".into())
                .spawn(move || {
                    let mut egress = match whip_ingest::MediaEgress::answer(&offer_sdp, &stun) {
                        Ok(e) => e,
                        Err(e) => {
                            log::warn!("[fast] egress answer failed: {e}");
                            let _ = ans_tx.send(None);
                            return;
                        }
                    };
                    let _ = ans_tx.send(Some(egress.answer_sdp().to_string()));
                    // Pump access units until the viewer leaves or the connection drops. The
                    // recv timeout doubles as a liveness tick: reap once a connected session
                    // goes down, or if it never comes up within the grace window.
                    let (mut connected_once, mut idle_ticks) = (false, 0u32);
                    loop {
                        match frame_rx.recv_timeout(Duration::from_millis(500)) {
                            Ok(FastFrame::Au(au, ts)) => {
                                egress.write_au(&au, ts);
                                if egress.is_connected() {
                                    connected_once = true;
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if egress.is_connected() {
                                    connected_once = true;
                                    idle_ticks = 0;
                                } else if connected_once {
                                    break; // was up, now gone
                                } else {
                                    idle_ticks += 1;
                                    if idle_ticks > 20 {
                                        break; // ~10s and never connected
                                    }
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    log::info!("[fast] egress for a viewer closed");
                })
                .ok()?;
            self.viewers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(viewer, frame_tx);
            match tokio::time::timeout(Duration::from_secs(4), ans_rx).await {
                Ok(Ok(Some(sdp))) => Some(sdp),
                _ => {
                    self.drop_viewer(&viewer);
                    None
                }
            }
        }
    }
}

/// Spawn the publisher's fast-tier accept loop: read fast-tier offers addressed to us, answer
/// each with a sendonly egress, and relay the answer back. Runs until the returned task is
/// aborted (publish teardown). Access units are pumped in via [`publisher::FastTier::broadcast`]
/// from the ingest feeder.
#[cfg(all(feature = "publish", not(target_os = "android")))]
pub fn spawn_accept_loop(
    signaling: unstation_chain::ChainSignaling,
    my_peer: PeerId,
    fast: std::sync::Arc<FastTier>,
) -> tokio::task::JoinHandle<()> {
    use std::collections::HashSet;
    tokio::spawn(async move {
        let mut push = signaling.subscribe_fast_signals_push(my_peer);
        // Offers we've already answered this session (dedup — the viewer resends until it
        // gets the answer, and statements linger on their ~30s TTL).
        let mut answered: HashSet<([u8; 32], Vec<u8>)> = HashSet::new();
        loop {
            if let Ok(sigs) = signaling.read_fast_signals(my_peer).await {
                for (viewer, msg) in sigs {
                    match msg {
                        SignalMsg::Offer { sdp } => {
                            let key = (viewer.0, sdp.clone());
                            if !answered.insert(key) {
                                continue; // already handled this exact offer
                            }
                            let offer = sdp_str(&sdp);
                            match fast.accept_offer(viewer.0, offer).await {
                                Some(answer) => {
                                    let _ = signaling
                                        .publish_fast_signal(
                                            my_peer,
                                            viewer,
                                            SignalMsg::Answer { offer_id: FAST_OFFER_ID.into(), sdp: sdp_bytes(&answer) },
                                        )
                                        .await;
                                }
                                None => {
                                    // Capped or failed → tell the viewer to fall back to the mesh.
                                    let _ = signaling
                                        .publish_fast_signal(
                                            my_peer,
                                            viewer,
                                            SignalMsg::Closed { offer_id: FAST_OFFER_ID.into() },
                                        )
                                        .await;
                                }
                            }
                        }
                        SignalMsg::Closed { .. } => {
                            fast.drop_viewer(&viewer.0);
                        }
                        _ => {}
                    }
                }
            }
            tokio::select! {
                _ = push.recv() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(700)) => {}
            }
        }
    })
}

/// Convert an access unit presentation time (µs, from the depacketizer) to a 90kHz RTP
/// timestamp for the egress packetizer.
pub fn pts_us_to_rtp90k(pts_us: i64) -> u32 {
    ((pts_us.max(0) as u64).wrapping_mul(90) / 1000) as u32
}
