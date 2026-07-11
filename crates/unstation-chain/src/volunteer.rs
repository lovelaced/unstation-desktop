//! Volunteer-seed rendezvous + recruitment over the statement store.
//!
//! The chain side of `unstation_core::volunteer`: an open seed announces a
//! [`VolunteerRecord`] on the global [`volunteers_topic`] and listens on its own
//! [`recruit_topic`] inbox; a publisher reads the rendezvous, picks volunteers, and
//! posts signed [`Recruitment`]s into their inboxes — sealed with the [`seal_to`]
//! framing so only the recruited volunteer learns which stream it was handed.

use parity_scale_codec::{Decode, Encode};
use unstation_core::topic::{recruit_topic, volunteers_topic};
use unstation_core::types::PeerId;
use unstation_core::volunteer::{Recruitment, VolunteerRecord, VOLUNTEER_VERSION};

use crate::{err, local_peer_id, open_sealed, seal_to, submit_counted, subscribe_topic};
use useragent_native::chain::statement_store as ss;

/// Base of the priority clock — a fixed, recent instant (June 2025, before any
/// deployment). Counting seconds from here instead of 1970 keeps the shifted value
/// inside u32 until ~2093.
const PRIO_EPOCH: u64 = 1_750_000_000;

/// Statement priority for a rendezvous record: seconds since [`PRIO_EPOCH`], shifted
/// one bit (channel = topic, last-write-wins by priority). Deriving prio from time —
/// instead of fixed announce=0 / tombstone=1 — means LATER always supersedes: a
/// restart's fresh announce beats the previous shutdown's tombstone, which a fixed
/// prio 1 would have masked forever. The tombstone takes the odd slot (`| 1`), so it
/// outranks the same second's announce while any strictly later announce outranks it.
fn announce_prio(issued_at: u64) -> u32 {
    ((issued_at.saturating_sub(PRIO_EPOCH) & 0x7FFF_FFFF) as u32) << 1
}

/// Announce this node as an open volunteer seed on the global rendezvous topic.
/// The announce loop re-publishes before `ttl_s` runs out (channel = topic, so the
/// newest record supersedes the last — see [`announce_prio`]).
pub async fn publish_volunteer(rec: VolunteerRecord) -> unstation_core::Result<()> {
    let topic = volunteers_topic();
    let prio = announce_prio(rec.issued_at);
    let data = rec.encode();
    // SDK statement-store calls are blocking (sync WS I/O) — keep them off the reactor.
    tokio::task::spawn_blocking(move || submit_counted(topic, &data, prio))
        .await
        .map_err(err)?
        .map_err(err)
}

/// Withdraw from the rendezvous (clean shutdown): republish the record as a
/// tombstone — `ttl_s = 1`, no capacity — at [`announce_prio`]'s odd slot, so it
/// beats an announce from the SAME second already in flight without outranking any
/// future restart's fresh announce. Readers drop it on both the age filter and
/// `max_streams == 0`.
pub async fn withdraw_volunteer(mut rec: VolunteerRecord) -> unstation_core::Result<()> {
    rec.ttl_s = 1;
    rec.max_streams = 0;
    rec.active_streams = 0;
    rec.issued_at = unix_now();
    let topic = volunteers_topic();
    let prio = announce_prio(rec.issued_at) | 1;
    let data = rec.encode();
    tokio::task::spawn_blocking(move || submit_counted(topic, &data, prio))
        .await
        .map_err(err)?
        .map_err(err)
}

/// Publisher: read up to `max` live volunteer records from the rendezvous topic.
/// Malformed statements are dropped; liveness is age-filtered against the record's
/// OWN `issued_at + ttl_s` (chain statements linger ~1 h past it) and tombstones
/// (`max_streams == 0`) are excluded.
pub async fn read_volunteers(
    max: usize,
    now_unix: u64,
) -> unstation_core::Result<Vec<VolunteerRecord>> {
    let topic = volunteers_topic();
    let statements = tokio::task::spawn_blocking(move || ss::rpc_get_broadcasts(&[topic]))
        .await
        .map_err(err)?
        .map_err(err)?;
    let raw: Vec<VolunteerRecord> = statements
        .into_iter()
        .filter_map(|st| VolunteerRecord::decode(&mut &st.data[..]).ok())
        .collect();
    Ok(filter_volunteers(raw, max, now_unix))
}

/// The pure liveness filter behind [`read_volunteers`] (split out so it is testable
/// without a chain): current version, unexpired by the record's own clock, and
/// offering capacity.
fn filter_volunteers(
    raw: Vec<VolunteerRecord>,
    max: usize,
    now_unix: u64,
) -> Vec<VolunteerRecord> {
    let mut out: Vec<VolunteerRecord> = raw
        .into_iter()
        .filter(|r| {
            r.version == VOLUNTEER_VERSION
                && r.issued_at.saturating_add(r.ttl_s as u64) >= now_unix
                && r.max_streams > 0
        })
        .collect();
    out.truncate(max);
    out
}

/// Publisher: post an already-SCALE-encoded, already-SIGNED [`Recruitment`] into the
/// volunteer's recruit inbox, sealed to its advertised X25519 key. `prio` orders a
/// publisher's successive recruitments/releases to the same volunteer (channel =
/// topic, last-write-wins by priority).
pub async fn publish_recruitment(
    to_enc_pub: &[u8; 32],
    to_peer: &[u8; 32],
    encoded_recruitment: &[u8],
    prio: u32,
) -> unstation_core::Result<()> {
    let topic = recruit_topic(&PeerId(*to_peer));
    let Some(sealed) = seal_to(to_enc_pub, encoded_recruitment) else {
        return Err(err("no local identity — cannot seal recruitment"));
    };
    tokio::task::spawn_blocking(move || submit_counted(topic, &sealed, prio))
        .await
        .map_err(err)?
        .map_err(err)
}

/// Volunteer: read every recruitment currently in OUR inbox that opens, decodes, and
/// passes [`Recruitment::verify`] for our peer id (freshness + the publisher's
/// signature over a payload that binds our inbox — see
/// `unstation_core::volunteer::Recruitment::signing_payload`). Failures are
/// log-warned and dropped, never surfaced as errors: a hostile statement in a public
/// inbox must not wedge the honest ones. Empty if no identity is initialized.
pub async fn read_recruitments(now_unix: u64) -> unstation_core::Result<Vec<Recruitment>> {
    let Some(me) = local_peer_id() else {
        return Ok(Vec::new());
    };
    let topic = recruit_topic(&me);
    let statements = tokio::task::spawn_blocking(move || ss::rpc_get_broadcasts(&[topic]))
        .await
        .map_err(err)?
        .map_err(err)?;
    let mut out = Vec::new();
    for st in statements {
        let Some((_, plaintext)) = open_sealed(&st.data) else {
            log::warn!("[volunteer] dropping recruitment that won't open (not sealed to us?)");
            continue;
        };
        let Ok(rec) = Recruitment::decode(&mut &plaintext[..]) else {
            log::warn!("[volunteer] dropping malformed recruitment");
            continue;
        };
        if let Err(reason) = rec.verify(&me, now_unix) {
            log::warn!("[volunteer] dropping recruitment: {reason}");
            continue;
        }
        out.push(rec);
    }
    Ok(out)
}

/// Volunteer: push wakeups for statements landing in our recruit inbox. The payloads
/// delivered on the receiver are still SEALED — this is a wakeup signal only, and the
/// caller re-reads via [`read_recruitments`] (one open/verify path, no drift) exactly
/// as the session uses its edge push. `None` if no identity is initialized yet.
pub fn subscribe_recruitments() -> Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
    Some(subscribe_topic(recruit_topic(&local_peer_id()?)))
}

/// Unix seconds now (for the tombstone's fresh `issued_at`).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_test_identity;
    use unstation_core::crypto;
    use unstation_core::volunteer::{RecruitAction, RECRUIT_CONTEXT};

    #[test]
    fn seal_to_open_sealed_round_trips_generic_bodies() {
        install_test_identity();
        let our_pub = crate::identity_enc_public().expect("enc key");

        // A generic (non-SignalMsg) body survives the framing; sealing to ourselves
        // works because static-static ECDH is symmetric.
        let plaintext = b"any bytes at all: recruitment or otherwise".to_vec();
        let sealed = seal_to(&our_pub, &plaintext).expect("seal");
        let (sender, opened) = open_sealed(&sealed).expect("open");
        assert_eq!(sender, our_pub, "opener learns the sender key for replies");
        assert_eq!(opened, plaintext);
        // The plaintext must not appear in the sealed bytes.
        assert!(sealed.windows(plaintext.len()).all(|w| w != &plaintext[..]));

        // Tamper, truncation, and a wrong framing tag all fail closed.
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_sealed(&tampered).is_none());
        assert!(open_sealed(&sealed[..20]).is_none());
        let mut wrong_tag = sealed;
        wrong_tag[0] = 0x00;
        assert!(open_sealed(&wrong_tag).is_none());
    }

    #[test]
    fn recruitment_signed_with_identity_ctx_verifies() {
        // The full publisher-side flow without a chain: sign with the host identity
        // under the recruit context, and check the volunteer-side verify accepts it
        // (and still rejects a replay into another inbox).
        let secret = install_test_identity();
        let kp = crate::keypair_from_secret(&secret).expect("valid test identity");
        let volunteer = PeerId([0x77u8; 32]);
        let now = 1_700_000_000u64;
        let mut rec = Recruitment {
            version: unstation_core::volunteer::RECRUITMENT_VERSION,
            stream_id: [0x44; 32],
            manifest_cid: "bafy-test".into(),
            publisher: crypto::public_bytes(&kp),
            issued_at: now,
            action: RecruitAction::Recruit,
            sig: [0u8; 64],
        };
        rec.sig = crate::sign_with_identity_ctx(RECRUIT_CONTEXT, &rec.signing_payload(&volunteer))
            .expect("identity installed");
        assert!(rec.verify(&volunteer, now).is_ok());
        assert!(rec.verify(&PeerId([0x78u8; 32]), now).is_err(), "inbox replay rejected");
    }

    fn vol(ttl_s: u32, issued_at: u64, max_streams: u32, version: u16) -> VolunteerRecord {
        VolunteerRecord {
            version,
            peer_id: [1; 32],
            account: [2; 32],
            enc_pub: [3; 32],
            caps_upload_bps: 1,
            active_streams: 0,
            max_streams,
            ttl_s,
            issued_at,
        }
    }

    #[test]
    fn announce_prio_makes_later_writes_win() {
        let t = 1_800_000_000u64; // a plausible runtime clock, past PRIO_EPOCH
        // A tombstone at shutdown outranks the announce from the same second …
        assert!((announce_prio(t) | 1) > announce_prio(t));
        // … but a restart's fresh announce (ANY later second) strictly outranks the
        // tombstone, so a re-announced seed is never masked by its own old withdrawal.
        assert!(announce_prio(t + 1) > (announce_prio(t) | 1));
        // Announces themselves stay monotone.
        assert!(announce_prio(t + 1) > announce_prio(t));
        // A pre-epoch clock (bad NTP) saturates instead of wrapping high.
        assert_eq!(announce_prio(0), 0);
        // The odd slot never overflows even at the mask ceiling (~year 2093).
        assert_eq!(announce_prio(PRIO_EPOCH + 0x7FFF_FFFF) | 1, u32::MAX);
    }

    #[test]
    fn filter_volunteers_drops_stale_tombstoned_and_alien_versions() {
        let now = 10_000u64;
        let raw = vec![
            vol(60, now - 30, 4, VOLUNTEER_VERSION),  // live
            vol(60, now - 60, 4, VOLUNTEER_VERSION),  // boundary: issued_at + ttl == now → live
            vol(60, now - 61, 4, VOLUNTEER_VERSION),  // expired by its own clock
            vol(60, now - 30, 0, VOLUNTEER_VERSION),  // tombstone (withdrawn)
            vol(60, now - 30, 4, VOLUNTEER_VERSION + 1), // future wire version
        ];
        let kept = filter_volunteers(raw.clone(), 16, now);
        assert_eq!(kept.len(), 2, "only the live, capacitated, v1 records survive");

        // `max` truncates.
        assert_eq!(filter_volunteers(raw, 1, now).len(), 1);
    }
}
