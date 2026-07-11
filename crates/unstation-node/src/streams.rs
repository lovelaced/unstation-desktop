//! Stream-name canonicalization + id derivation, shared by main.rs (pins) and the
//! supervisor. Moved verbatim from main.rs — the contract with the app is unchanged.

use unstation_core::crypto;
use unstation_core::types::StreamId;

/// Same canonicalization as the app's `canonical_stream_name`: lowercase, runs of
/// non-alphanumerics → single hyphens, `.dot` suffix dropped, empty → "my-stream".
/// The two MUST agree byte-for-byte or the seed joins the wrong (empty) swarm.
pub fn canonical_stream_name(input: &str) -> String {
    let s = input.trim();
    let s = s.strip_suffix(".dot").unwrap_or(s);
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "my-stream".into()
    } else {
        out
    }
}

pub fn stream_id_from(name: &str) -> StreamId {
    StreamId(crypto::blake2b256(canonical_stream_name(name).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalization_matches_the_app() {
        assert_eq!(canonical_stream_name("My Stream.dot"), "my-stream");
        assert_eq!(canonical_stream_name("  jet__live!  "), "jet-live");
        assert_eq!(canonical_stream_name("---"), "my-stream");
        assert_eq!(canonical_stream_name(""), "my-stream");
        assert_eq!(canonical_stream_name("seed-e2e"), "seed-e2e");
    }

    #[test]
    fn stream_id_is_the_hash_of_the_canonical_name() {
        // Different spellings of the same canonical name land in the same swarm.
        assert_eq!(stream_id_from("My Stream"), stream_id_from("my-stream.dot"));
        assert_ne!(stream_id_from("a"), stream_id_from("b"));
    }
}
