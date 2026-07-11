//! Recruit-inbox listener: turns the chain's raw (already signature-verified)
//! recruitments into [`VerifiedRecruitment`]s the supervisor can act on.
//!
//! `unstation_chain::read_recruitments` verifies freshness + the publisher's
//! signature (bound to OUR inbox) — but statements linger on the chain (~1 h, with a
//! ±600 s freshness window), so every read returns the same recruitments again: this
//! module dedups by `(publisher, stream, issued_at, action)`. For a Recruit it ALSO
//! fetches the named manifest and checks it verifies against the recruiting
//! publisher AND names the recruited stream — so a supervisor never spins up a
//! worker for a stream whose manifest a hostile publisher can't actually sign.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use unstation_chain::BulletinOrigin;
use unstation_core::crypto;
use unstation_core::manifest::OriginOfRecord;
use unstation_core::types::StreamId;
use unstation_core::volunteer::{RecruitAction, Recruitment, RECRUIT_MAX_SKEW_S};

/// Reconciliation cadence for the inbox read (push wakeups cover the fast path).
const RECRUIT_POLL: Duration = Duration::from_secs(30);
/// Heartbeat-log name prefix length for a recruited stream (no canonical name known).
const HINT_HEX_LEN: usize = 16;

/// A recruitment that passed every check the supervisor shouldn't have to repeat:
/// signature + freshness (chain layer), and for Recruit the manifest fetch +
/// publisher-signature + stream-id binding (here).
pub struct VerifiedRecruitment {
    pub stream: StreamId,
    /// Display name stand-in — a hex prefix of the stream id (recruitments carry no
    /// canonical name, and the id is the hash of one we can't invert).
    pub canon_hint: String,
    pub publisher: [u8; 32],
    pub manifest_cid: String,
    /// From the VERIFIED manifest's `target_segment_ms` (1000 if the manifest says 0).
    pub seg_ms: u32,
    pub action: RecruitAction,
    pub issued_at: u64,
}

/// Listen on our recruit inbox and forward verified recruitments to `tx`. Manifest
/// fetches are awaited inline — recruitments are rare and ordering per publisher
/// matters more than latency here.
pub fn spawn_recruit_listener(tx: UnboundedSender<VerifiedRecruitment>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Push wakeups for statements landing in the inbox; payloads arrive SEALED, so
        // this is a wakeup only — the read below is the single open/verify path.
        // None only if no identity is initialized (retry each tick).
        let mut push = unstation_chain::volunteer::subscribe_recruitments();
        let mut seen: HashSet<([u8; 32], [u8; 32], u64, u8)> = HashSet::new();
        let mut tick = tokio::time::interval(RECRUIT_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            match push.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        _ = tick.tick() => {}
                        Some(_) = rx.recv() => {
                            while rx.try_recv().is_ok() {} // coalesce a burst into one read
                        }
                    }
                }
                None => {
                    tick.tick().await;
                    push = unstation_chain::volunteer::subscribe_recruitments();
                }
            }
            let now = unix_now();
            let recs = match unstation_chain::volunteer::read_recruitments(now).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[recruit] inbox read failed: {e:?}");
                    continue;
                }
            };
            // Entries older than twice the freshness window can never re-verify —
            // safe to forget, keeping the dedup set bounded.
            seen.retain(|(_, _, issued_at, _)| now.saturating_sub(*issued_at) <= 2 * RECRUIT_MAX_SKEW_S);
            for rec in recs {
                if !seen.insert(dedup_key(&rec)) {
                    continue; // lingering statement, already handled
                }
                if let Some(vr) = verify_recruitment(rec).await {
                    if tx.send(vr).is_err() {
                        return; // supervisor gone — nothing left to feed
                    }
                }
            }
        }
    })
}

fn dedup_key(rec: &Recruitment) -> ([u8; 32], [u8; 32], u64, u8) {
    let action = match rec.action {
        RecruitAction::Recruit => 0,
        RecruitAction::Release => 1,
    };
    (rec.publisher, rec.stream_id, rec.issued_at, action)
}

/// The manifest leg of trust: a Recruit is forwarded only if the CID it names
/// resolves to a manifest that (a) verifies against the recruiting publisher and
/// (b) is FOR the recruited stream — otherwise a signer could point us at someone
/// else's manifest or recruit us onto a stream it doesn't publish. A Release needs
/// no manifest: worst case we idle a worker out slightly early.
async fn verify_recruitment(rec: Recruitment) -> Option<VerifiedRecruitment> {
    let canon_hint = crypto::hex32(&rec.stream_id)[..HINT_HEX_LEN].to_string();
    let seg_ms = match rec.action {
        RecruitAction::Release => 0,
        RecruitAction::Recruit => {
            let m = match BulletinOrigin.fetch_manifest(rec.manifest_cid.clone()).await {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("[recruit] dropping recruit for {canon_hint}…: manifest fetch failed ({e:?})");
                    return None;
                }
            };
            if let Err(e) = m.verify(&rec.publisher) {
                log::warn!("[recruit] dropping recruit for {canon_hint}…: manifest doesn't verify against the recruiting publisher ({e:?})");
                return None;
            }
            if m.manifest.stream_id.0 != rec.stream_id {
                log::warn!("[recruit] dropping recruit for {canon_hint}…: manifest names a different stream");
                return None;
            }
            if m.manifest.target_segment_ms > 0 { m.manifest.target_segment_ms } else { 1000 }
        }
    };
    Some(VerifiedRecruitment {
        stream: StreamId(rec.stream_id),
        canon_hint,
        publisher: rec.publisher,
        manifest_cid: rec.manifest_cid,
        seg_ms,
        action: rec.action,
        issued_at: rec.issued_at,
    })
}

/// Unix seconds now (the freshness clock `read_recruitments` verifies against).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(publisher: u8, stream: u8, issued_at: u64, action: RecruitAction) -> Recruitment {
        Recruitment {
            version: unstation_core::volunteer::RECRUITMENT_VERSION,
            stream_id: [stream; 32],
            manifest_cid: "bafy-test".into(),
            publisher: [publisher; 32],
            issued_at,
            action,
            sig: [0u8; 64],
        }
    }

    #[test]
    fn dedup_key_separates_every_field_that_makes_a_new_assignment() {
        let base = rec(1, 2, 100, RecruitAction::Recruit);
        assert_eq!(dedup_key(&base), dedup_key(&rec(1, 2, 100, RecruitAction::Recruit)));
        // Publisher, stream, issue time, and action each distinguish.
        assert_ne!(dedup_key(&base), dedup_key(&rec(3, 2, 100, RecruitAction::Recruit)));
        assert_ne!(dedup_key(&base), dedup_key(&rec(1, 3, 100, RecruitAction::Recruit)));
        assert_ne!(dedup_key(&base), dedup_key(&rec(1, 2, 101, RecruitAction::Recruit)));
        assert_ne!(dedup_key(&base), dedup_key(&rec(1, 2, 100, RecruitAction::Release)));
    }
}
