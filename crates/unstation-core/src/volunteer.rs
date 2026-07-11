//! Stream-agnostic volunteer seeding: the rendezvous + recruitment wire types.
//!
//! An OPEN volunteer seed runs with no stream configured. It announces a
//! [`VolunteerRecord`] on the global [`volunteers_topic`]; a publisher going live
//! reads that topic, picks volunteers with spare capacity, and posts a signed
//! [`Recruitment`] into each one's per-peer [`recruit_topic`] inbox (sealed to the
//! volunteer's X25519 key by the chain layer). The volunteer verifies the publisher's
//! signature and joins — no operator ever has to name a stream up front.
//!
//! [`volunteers_topic`]: crate::topic::volunteers_topic
//! [`recruit_topic`]: crate::topic::recruit_topic

use crate::crypto;
use crate::types::PeerId;
use parity_scale_codec::{Decode, Encode};

/// Wire version of [`VolunteerRecord`] (readers drop other versions).
pub const VOLUNTEER_VERSION: u16 = 1;

/// Wire version of [`Recruitment`] ([`Recruitment::verify`] rejects other versions).
pub const RECRUITMENT_VERSION: u16 = 1;

/// Signing-context label for recruitment signatures — domain-separated from
/// [`crypto::MANIFEST_CONTEXT`], so a recruitment can never be replayed as a
/// manifest/edge signature or vice versa.
pub const RECRUIT_CONTEXT: &[u8] = b"unstation-recruit";

/// Maximum accepted |now − issued_at| for a [`Recruitment`], in seconds. Statements
/// linger on the chain well past their usefulness; anything staler than this is a
/// replay or a leftover, not a live assignment.
pub const RECRUIT_MAX_SKEW_S: u64 = 600;

/// An open volunteer seed's capacity announcement on the global volunteers topic
/// (SCALE wire form, the statement `data` payload).
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub struct VolunteerRecord {
    /// Wire version — readers drop anything but [`VOLUNTEER_VERSION`].
    pub version: u16,
    /// The seed supervisor's process-stable mesh routing id — the address of its
    /// [`recruit_topic`](crate::topic::recruit_topic) inbox. NOT a signing key.
    pub peer_id: [u8; 32],
    /// The volunteer's personhood/statement-store pubkey — dedups one operator's
    /// re-announcements and attributes the capacity it contributes.
    pub account: [u8; 32],
    /// X25519 key recruitments are SEALED to, so a stream assignment (which names the
    /// publisher and stream) is readable only by the recruited volunteer.
    pub enc_pub: [u8; 32],
    /// Self-reported upload capacity, for the publisher's volunteer picking.
    pub caps_upload_bps: u64,
    /// Streams this volunteer is currently seeding.
    pub active_streams: u32,
    /// Streams this volunteer is willing to seed at once; `0` is a tombstone
    /// (withdrawn — readers filter it out).
    pub max_streams: u32,
    /// Announce validity window in seconds, from `issued_at`.
    pub ttl_s: u32,
    /// Unix seconds at announce time. Readers age-filter on `issued_at + ttl_s`
    /// themselves: chain statements linger ~1 h past the record's own `ttl_s`, so
    /// the store's retention cannot be trusted as the liveness signal.
    pub issued_at: u64,
}

/// What a [`Recruitment`] asks the volunteer to do with the stream.
#[derive(Encode, Decode, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecruitAction {
    /// Start seeding the stream (SCALE `0x00`).
    Recruit,
    /// Stop seeding it — the publisher went offline or scaled down (SCALE `0x01`).
    Release,
}

/// A publisher's signed stream assignment, posted (sealed) into one volunteer's
/// recruit inbox.
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub struct Recruitment {
    /// Wire version — [`Recruitment::verify`] rejects anything but
    /// [`RECRUITMENT_VERSION`].
    pub version: u16,
    /// The stream to seed.
    pub stream_id: [u8; 32],
    /// Bulletin CID of the stream's signed manifest, so the volunteer can verify the
    /// stream against `publisher` before seeding a byte of it.
    pub manifest_cid: String,
    /// The publisher's personhood pubkey — the trust anchor `sig` verifies against.
    pub publisher: [u8; 32],
    /// Unix seconds at issue time; [`Recruitment::verify`] bounds the skew by
    /// [`RECRUIT_MAX_SKEW_S`] so lingering statements can't re-recruit later.
    pub issued_at: u64,
    /// Recruit or release.
    pub action: RecruitAction,
    /// sr25519 signature by `publisher` under [`RECRUIT_CONTEXT`] over
    /// [`Recruitment::signing_payload`].
    pub sig: [u8; 64],
}

impl Recruitment {
    /// The canonical bytes `sig` covers: SCALE of every field except `sig`, followed
    /// by the TARGET volunteer's peer id. The volunteer peer id is SIGNED but NOT on
    /// the wire — it is implied by which recruit-inbox topic the message was posted
    /// to — so a volunteer cannot take a recruitment addressed to it and replay it
    /// into ANOTHER volunteer's inbox (the signature won't verify there).
    pub fn signing_payload(&self, volunteer_peer: &PeerId) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + 32 + 1 + self.manifest_cid.len() + 32 + 8 + 1 + 32);
        self.version.encode_to(&mut buf);
        self.stream_id.encode_to(&mut buf);
        self.manifest_cid.encode_to(&mut buf);
        self.publisher.encode_to(&mut buf);
        self.issued_at.encode_to(&mut buf);
        self.action.encode_to(&mut buf);
        buf.extend_from_slice(&volunteer_peer.0);
        buf
    }

    /// Accept this recruitment only if it is current-version, fresh (skew within
    /// [`RECRUIT_MAX_SKEW_S`]), addressed to `volunteer_peer` (see
    /// [`Recruitment::signing_payload`]), and signed by its own `publisher` under
    /// [`RECRUIT_CONTEXT`]. Fails closed with a reason for the caller to log.
    pub fn verify(&self, volunteer_peer: &PeerId, now_unix: u64) -> Result<(), &'static str> {
        if self.version != RECRUITMENT_VERSION {
            return Err("unsupported recruitment version");
        }
        if now_unix.abs_diff(self.issued_at) > RECRUIT_MAX_SKEW_S {
            return Err("recruitment outside freshness window");
        }
        if !crypto::verify_sr25519_ctx(
            RECRUIT_CONTEXT,
            &self.publisher,
            &self.signing_payload(volunteer_peer),
            &self.sig,
        ) {
            return Err("bad recruitment signature");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn sample_record() -> VolunteerRecord {
        VolunteerRecord {
            version: VOLUNTEER_VERSION,
            peer_id: [0x11; 32],
            account: [0x22; 32],
            enc_pub: [0x33; 32],
            caps_upload_bps: 20_000_000,
            active_streams: 2,
            max_streams: 8,
            ttl_s: 60,
            issued_at: 1_700_000_000,
        }
    }

    fn sample_recruitment(sig: [u8; 64]) -> Recruitment {
        Recruitment {
            version: RECRUITMENT_VERSION,
            stream_id: [0x44; 32],
            manifest_cid: "bafy-test".into(),
            publisher: [0x55; 32],
            issued_at: 1_700_000_000,
            action: RecruitAction::Recruit,
            sig,
        }
    }

    #[test]
    fn volunteer_record_wire_format_is_frozen() {
        // Pinned encoding of a fixed record: any change to field order, types, or
        // SCALE derive behavior breaks this test — the wire format is frozen at v1.
        let rec = sample_record();
        let expected = concat!(
            "0100",                                                             // version
            "1111111111111111111111111111111111111111111111111111111111111111", // peer_id
            "2222222222222222222222222222222222222222222222222222222222222222", // account
            "3333333333333333333333333333333333333333333333333333333333333333", // enc_pub
            "002d310100000000",                                                 // caps_upload_bps
            "02000000",                                                         // active_streams
            "08000000",                                                         // max_streams
            "3c000000",                                                         // ttl_s
            "00f1536500000000",                                                 // issued_at
        );
        let bytes = rec.encode();
        assert_eq!(hex(&bytes), expected);
        assert_eq!(VolunteerRecord::decode(&mut &bytes[..]).unwrap(), rec);
    }

    #[test]
    fn recruitment_wire_format_is_frozen() {
        let rec = sample_recruitment([0x66; 64]);
        let expected = concat!(
            "0100",                                                             // version
            "4444444444444444444444444444444444444444444444444444444444444444", // stream_id
            "24626166792d74657374",                                             // manifest_cid (compact-len ‖ utf8)
            "5555555555555555555555555555555555555555555555555555555555555555", // publisher
            "00f1536500000000",                                                 // issued_at
            "00",                                                               // action = Recruit
            "6666666666666666666666666666666666666666666666666666666666666666", // sig
            "6666666666666666666666666666666666666666666666666666666666666666",
        );
        let bytes = rec.encode();
        assert_eq!(hex(&bytes), expected);
        assert_eq!(Recruitment::decode(&mut &bytes[..]).unwrap(), rec);

        // The action discriminants are part of the frozen format: 0x00 / 0x01.
        assert_eq!(RecruitAction::Recruit.encode(), vec![0x00]);
        assert_eq!(RecruitAction::Release.encode(), vec![0x01]);
    }

    /// A signed recruitment addressed to `volunteer`, from the keypair seeded with 9s.
    fn signed(volunteer: &PeerId, issued_at: u64) -> (Recruitment, [u8; 32]) {
        let kp = crypto::keypair_from_seed(&[9u8; 32]);
        let publisher = crypto::public_bytes(&kp);
        let mut rec = sample_recruitment([0u8; 64]);
        rec.publisher = publisher;
        rec.issued_at = issued_at;
        rec.sig = crypto::sign_sr25519_ctx(&kp, RECRUIT_CONTEXT, &rec.signing_payload(volunteer));
        (rec, publisher)
    }

    #[test]
    fn recruitment_verify_matrix() {
        let volunteer = PeerId([0x77; 32]);
        let now = 1_700_000_000u64;
        let (rec, _) = signed(&volunteer, now);

        // Happy path, including modest skew in both directions.
        assert!(rec.verify(&volunteer, now).is_ok());
        assert!(rec.verify(&volunteer, now + RECRUIT_MAX_SKEW_S).is_ok());
        assert!(rec.verify(&volunteer, now - RECRUIT_MAX_SKEW_S).is_ok());

        // Expired: past the freshness window in either direction.
        assert!(rec.verify(&volunteer, now + RECRUIT_MAX_SKEW_S + 1).is_err());
        assert!(rec.verify(&volunteer, now - RECRUIT_MAX_SKEW_S - 1).is_err());

        // Bad signature bit.
        let mut bad_sig = rec.clone();
        bad_sig.sig[0] ^= 1;
        assert!(bad_sig.verify(&volunteer, now).is_err());

        // Wrong context: a signature made under MANIFEST_CONTEXT must not verify.
        let kp = crypto::keypair_from_seed(&[9u8; 32]);
        let mut wrong_ctx = rec.clone();
        wrong_ctx.sig = crypto::sign_sr25519(&kp, &wrong_ctx.signing_payload(&volunteer));
        assert!(wrong_ctx.verify(&volunteer, now).is_err());

        // Replay into another volunteer's inbox: the target peer id is signed, so the
        // same bytes fail verification under any other volunteer.
        assert!(rec.verify(&PeerId([0x78; 32]), now).is_err());

        // Wrong version is rejected before any crypto.
        let mut v2 = rec.clone();
        v2.version = 2;
        assert!(v2.verify(&volunteer, now).is_err());

        // Tampered content (e.g. a swapped stream) breaks the signature.
        let mut tampered = rec;
        tampered.stream_id = [0x45; 32];
        assert!(tampered.verify(&volunteer, now).is_err());
    }

    #[test]
    fn signing_payload_binds_the_volunteer_but_not_the_sig() {
        let (rec, _) = signed(&PeerId([1u8; 32]), 1_700_000_000);
        // Different target volunteer → different payload (the anti-replay binding) …
        assert_ne!(
            rec.signing_payload(&PeerId([1u8; 32])),
            rec.signing_payload(&PeerId([2u8; 32]))
        );
        // … and the sig itself is not part of what is signed.
        let mut resigned = rec.clone();
        resigned.sig = [0xAA; 64];
        assert_eq!(
            rec.signing_payload(&PeerId([1u8; 32])),
            resigned.signing_payload(&PeerId([1u8; 32]))
        );
    }
}
