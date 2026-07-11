//! Key-dir layout + identity precedence for the seed's on-chain identity.
//!
//! Files (all owned by the user running the seed):
//!   slot_secret       raw 32- or 64-byte sr25519 slot secret written by `pair`
//!                     (the phone granted this account a statement-store allowance)
//!   sso_secrets.json  V2 pairing device identity (owned by unstation-chain::sso_pair)
//!   sso_session.json  paired-session metadata (owned by unstation-chain::sso_pair)
//!   peer_key          legacy generated key, written by the SDK on dev chains
//!
//! Precedence is deliberate: a paired slot key always wins (it is the only identity
//! that works on the public chain), an operator-provided mnemonic overrides the
//! legacy generated key, and the generated key remains for dev chains where the e2e
//! harness provisions it out-of-band. A corrupt slot file is a hard error, never a
//! silent fall-through to a different identity — that would announce the seed under
//! an unprovisioned key and strand it invisibly.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

pub const SLOT_SECRET_FILE: &str = "slot_secret";
/// The seed's product id for phone-granted allowance slots. Distinct from the
/// desktop app's `unstation-live` so a person's desktop and seed never share an
/// on-chain identity.
pub const SEED_PRODUCT_ID: &str = "unstation-seed";

pub enum IdentitySource {
    /// `slot_secret` contents — a phone-paired, allowance-backed slot key.
    PairedSlot(Zeroizing<Vec<u8>>),
    /// `UNSTATION_NODE_MNEMONIC` — a pre-provisioned account.
    Mnemonic(String),
    /// No slot, no mnemonic: the SDK's load-or-generate `peer_key` path.
    GeneratedLegacy,
}

/// The key dir: `UNSTATION_NODE_KEY_DIR`, defaulting to `~/.unstation-node`.
pub fn default_key_dir() -> PathBuf {
    match std::env::var("UNSTATION_NODE_KEY_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".unstation-node")
        }
    }
}

/// Read + validate the persisted slot secret. `Ok(None)` if the file doesn't exist;
/// `Err` if it exists but is unreadable or not 32/64 bytes (corrupt — surface it,
/// don't fall through).
pub fn read_slot_secret(key_dir: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let path = key_dir.join(SLOT_SECRET_FILE);
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut secret = Zeroizing::new(Vec::with_capacity(64));
    file.read_to_end(&mut secret)?;
    if secret.len() != 32 && secret.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is {} bytes (expected 32 or 64) — corrupt? re-run `unstation-node pair --force`",
                path.display(),
                secret.len()
            ),
        ));
    }
    Ok(Some(secret))
}

/// Persist the slot secret: key dir created 0700 if absent (an existing dir's modes
/// are left alone — the installer sets up /var/lib/unstation-seed itself), file
/// written 0600 and fsynced. Overwrites an existing secret (`pair --force`).
pub fn write_slot_secret(key_dir: &Path, secret: &[u8]) -> io::Result<()> {
    if secret.len() != 32 && secret.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("slot secret must be 32 or 64 bytes, got {}", secret.len()),
        ));
    }
    if !key_dir.exists() {
        std::fs::create_dir_all(key_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key_dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let path = key_dir.join(SLOT_SECRET_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    // An existing file keeps its old mode; enforce 0600 even on overwrite.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(secret)?;
    file.sync_all()
}

/// Pick the identity for this run. `mnemonic_env` is `UNSTATION_NODE_MNEMONIC`
/// (passed in, not read here, so tests don't touch process env).
pub fn resolve_identity(
    key_dir: &Path,
    mnemonic_env: Option<&str>,
) -> io::Result<IdentitySource> {
    if let Some(secret) = read_slot_secret(key_dir)? {
        return Ok(IdentitySource::PairedSlot(secret));
    }
    match mnemonic_env.map(str::trim) {
        Some(m) if !m.is_empty() => Ok(IdentitySource::Mnemonic(m.to_string())),
        _ => Ok(IdentitySource::GeneratedLegacy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("unstation-keydir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_dir_resolves_to_legacy() {
        let dir = tmp_dir("legacy");
        match resolve_identity(&dir, None).unwrap() {
            IdentitySource::GeneratedLegacy => {}
            _ => panic!("expected GeneratedLegacy"),
        }
    }

    #[test]
    fn mnemonic_beats_legacy_but_not_slot() {
        let dir = tmp_dir("precedence");
        match resolve_identity(&dir, Some("word ".repeat(12).trim())).unwrap() {
            IdentitySource::Mnemonic(m) => assert!(m.starts_with("word")),
            _ => panic!("expected Mnemonic"),
        }
        write_slot_secret(&dir, &[7u8; 64]).unwrap();
        match resolve_identity(&dir, Some("word")).unwrap() {
            IdentitySource::PairedSlot(s) => assert_eq!(s.len(), 64),
            _ => panic!("slot secret must take precedence over the mnemonic"),
        }
    }

    #[test]
    fn blank_mnemonic_is_ignored() {
        let dir = tmp_dir("blank");
        match resolve_identity(&dir, Some("   ")).unwrap() {
            IdentitySource::GeneratedLegacy => {}
            _ => panic!("expected GeneratedLegacy for a blank mnemonic"),
        }
    }

    #[test]
    fn slot_secret_round_trips_at_0600() {
        let dir = tmp_dir("roundtrip");
        write_slot_secret(&dir, &[42u8; 32]).unwrap();
        let got = read_slot_secret(&dir).unwrap().expect("present");
        assert_eq!(&got[..], &[42u8; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SLOT_SECRET_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "slot secret must be 0600");
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700, "fresh key dir must be 0700");
        }
    }

    #[test]
    fn overwrite_works_for_force_repair() {
        let dir = tmp_dir("force");
        write_slot_secret(&dir, &[1u8; 64]).unwrap();
        write_slot_secret(&dir, &[2u8; 64]).unwrap();
        assert_eq!(&read_slot_secret(&dir).unwrap().unwrap()[..], &[2u8; 64]);
    }

    #[test]
    fn corrupt_slot_is_a_hard_error_not_a_fallthrough() {
        let dir = tmp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SLOT_SECRET_FILE), [9u8; 17]).unwrap();
        let err = read_slot_secret(&dir).unwrap_err();
        assert!(err.to_string().contains("pair --force"), "error should name the fix: {err}");
        assert!(resolve_identity(&dir, Some("mnemonic words")).is_err(),
            "resolve_identity must not fall through past a corrupt slot file");
    }

    #[test]
    fn wrong_length_write_is_rejected() {
        let dir = tmp_dir("badlen");
        assert!(write_slot_secret(&dir, &[0u8; 33]).is_err());
    }
}
