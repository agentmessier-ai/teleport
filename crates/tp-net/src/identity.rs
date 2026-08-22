//! Device identity (LLD §8.2): ed25519 keypair, generated once at install.
//! Device ID = base32 of the public-key fingerprint (blake3), grouped for
//! out-of-band comparison. The private key lives at `~/.teleport/key` (0600),
//! NOT in the Keychain — the LaunchAgent must work before first GUI unlock.

use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Identity {
    pub signing: SigningKey,
    pub verifying: VerifyingKey,
    /// base32 of blake3(pubkey), grouped `K7QX-2M4A-…`
    pub device_id: String,
}

impl Identity {
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying = signing.verifying_key();
        let device_id = fingerprint(&verifying);
        Self {
            signing,
            verifying,
            device_id,
        }
    }

    /// Load an existing keypair, or generate and persist one if absent.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let signing = SigningKey::from_bytes(
                &bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("bad key length"))?,
            );
            let verifying = signing.verifying_key();
            let device_id = fingerprint(&verifying);
            Ok(Self {
                signing,
                verifying,
                device_id,
            })
        } else {
            let id = Self::generate();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            save_key(path, &id.signing)?;
            Ok(id)
        }
    }

    /// This identity as an RFC 9421 (`httpsig`) signing key — see `auth::sign_request`.
    /// Ed25519 secret keys are 32-byte seeds; `SecretKey::from_bytes` on a
    /// well-formed 32-byte seed cannot fail, so this doesn't return `Result`.
    pub fn httpsig_secret_key(&self) -> httpsig::prelude::SecretKey {
        httpsig::prelude::SecretKey::from_bytes(
            &httpsig::prelude::AlgorithmName::Ed25519,
            &self.signing.to_bytes(),
        )
        .expect("32-byte ed25519 seed is always valid")
    }
}

/// A peer's verifying key as an RFC 9421 (`httpsig`) public key — see
/// `auth::verify_request`. Ed25519 public keys are 32 bytes; `PublicKey::from_bytes`
/// on a well-formed `VerifyingKey`'s bytes cannot fail.
pub fn httpsig_public_key(vk: &VerifyingKey) -> httpsig::prelude::PublicKey {
    httpsig::prelude::PublicKey::from_bytes(
        &httpsig::prelude::AlgorithmName::Ed25519,
        vk.as_bytes(),
    )
    .expect("32-byte ed25519 public key is always valid")
}

/// base32(blake3(pubkey)), grouped. Exposed so callers can check that a
/// claimed `device_id` actually matches the pubkey it arrived with (A3):
/// without this check an attacker can register any `device_id` string
/// alongside a pubkey they control, defeating out-of-band fingerprint
/// comparison — a human who diffs device_ids has no guarantee the id says
/// anything about the key that will actually be used to sign future requests.
pub fn fingerprint(vk: &VerifyingKey) -> String {
    let hash = blake3::hash(vk.as_bytes());
    let b32 = base32::encode(
        base32::Alphabet::Rfc4648 { padding: false },
        hash.as_bytes(),
    );
    // Group into 4-char chunks for comparison.
    let mut out = String::new();
    for (i, c) in b32.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

fn save_key(path: &Path, key: &SigningKey) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, key.to_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// This machine's display name. Lives beside `device_id` on purpose: the two
/// are the answer to the same question ("who is this Mac"), and duplicating
/// this with a different fallback string — as `server.rs` and the CLI once
/// each did — makes `/v1/ping` report a name that disagrees with the one
/// persisted in `machine.name`.
pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-machine".to_string())
}

pub fn default_key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".teleport").join("key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrip_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let id1 = Identity::load_or_create(&path).unwrap();
        let id2 = Identity::load_or_create(&path).unwrap();
        assert_eq!(
            id1.device_id, id2.device_id,
            "reload must yield the same identity"
        );
        assert_eq!(id1.verifying, id2.verifying);
        // Key file must be 0600.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "private key must be owner-only");
    }

    #[test]
    fn httpsig_key_conversion_round_trips() {
        use httpsig::prelude::{SigningKey as _, VerifyingKey as _};

        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(&dir.path().join("key")).unwrap();

        let sk = id.httpsig_secret_key();
        let pk = httpsig_public_key(&id.verifying);
        let data = b"round-trip check";
        let sig = sk.sign(data).unwrap();
        assert!(pk.verify(data, &sig).is_ok(), "signature made with the converted secret key must verify against the converted public key");

        let stranger = Identity::generate();
        let stranger_pk = httpsig_public_key(&stranger.verifying);
        assert!(
            stranger_pk.verify(data, &sig).is_err(),
            "a different identity's public key must not verify this signature"
        );
    }

    #[test]
    fn device_id_is_stable_and_grouped() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(&dir.path().join("key")).unwrap();
        assert!(
            id.device_id.contains('-'),
            "device id must be grouped: {}",
            id.device_id
        );
        assert!(
            id.device_id.len() >= 20,
            "id must be long enough to compare: {}",
            id.device_id
        );
    }
}
