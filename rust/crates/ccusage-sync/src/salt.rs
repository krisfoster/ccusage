//! The per-bucket secret that stands between the bucket's contents and a
//! dictionary attack.
//!
//! Everything hashed into a synced object — project paths, dedupe keys — is
//! hashed with this salt. Unsalted, `sha256("~/dev/<company>-<repo>")` is
//! reversible by anyone willing to enumerate plausible paths, which is a small
//! space, so the hashes would leak exactly what they exist to hide.
//!
//! The salt is minted once per bucket at `sync setup`, stored in the bucket's
//! private prefix, and mirrored into local config. Two machines that disagree
//! about it produce hashes that never intersect, so a mismatch is a hard stop
//! rather than something to paper over: silently continuing would double-count
//! every entry both machines saw.

use sha2::{Digest, Sha256};

use crate::identity::os_entropy;
use crate::shard::DedupeKey;

/// 128 bits, the same budget as the identifiers, rendered as hex.
const SALT_BYTES: usize = 16;
const SALT_HEX_LENGTH: usize = SALT_BYTES * 2;

/// Names the construction so a future change to it is visible in the data
/// rather than silently producing keys that no longer intersect.
pub const DEDUPE_ALGORITHM: &str = "sha256-64/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaltError {
    Malformed {
        value: String,
    },
    /// The bucket and the local config disagree. Hashing with the wrong salt
    /// produces keys that never match the other machine's, so every shared
    /// entry would be counted twice.
    Mismatch {
        bucket: String,
        configured: String,
    },
}

impl std::fmt::Display for SaltError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { value } => write!(
                formatter,
                "sync salt is not {SALT_HEX_LENGTH} hex characters: {value:?}"
            ),
            Self::Mismatch { bucket, configured } => write!(
                formatter,
                "this machine's sync salt ({configured}) is not the one the bucket was set up with ({bucket}). Usage hashed with a different salt cannot be deduplicated, so syncing would double-count shared activity. Remove 'sync.salt' from your config to adopt the bucket's salt."
            ),
        }
    }
}

impl std::error::Error for SaltError {}

/// Where the salt in use came from, so `sync status` can explain itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaltOrigin {
    /// Read from the bucket: this machine is joining an existing dataset.
    Bucket,
    /// Only local config had it; the next sync writes it to the bucket.
    Configured,
    Minted,
}

/// Secret by construction: it is never rendered by `Debug`, because a salt in a
/// log or a bug report undoes the hashing it protects.
#[derive(Clone, Eq, PartialEq)]
pub struct Salt(String);

impl std::fmt::Debug for Salt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Salt(<redacted>)")
    }
}

impl Salt {
    pub fn mint() -> Self {
        Self(
            os_entropy()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn parse(value: &str) -> Result<Self, SaltError> {
        let malformed = value.len() != SALT_HEX_LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if malformed {
            return Err(SaltError::Malformed {
                value: value.to_string(),
            });
        }
        Ok(Self(value.to_string()))
    }

    /// The value to persist. Callers must keep it out of logs and output.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A short public name for the salt, safe to store beside the data it
    /// salted: enough to detect a mismatch, not enough to reverse a hash.
    pub fn fingerprint(&self) -> String {
        let digest = digest(&[b"salt-fingerprint", self.0.as_bytes()]);
        digest
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The project path as it appears in a shard. The path itself never leaves
    /// the machine.
    pub fn hash_project(&self, project_path: &str) -> String {
        let digest = digest(&[b"project", self.0.as_bytes(), project_path.as_bytes()]);
        let hex: String = digest
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    /// The key two machines intersect to find entries they both reported.
    ///
    /// Truncated to 64 bits: the inputs are already unique per entry, and a
    /// day's worth of keys at this width collides with negligible probability
    /// while keeping a shard small enough to be worth shipping.
    pub fn dedupe_key(&self, message_id: &str, request_id: &str) -> DedupeKey {
        let digest = digest(&[
            b"dedupe",
            self.0.as_bytes(),
            message_id.as_bytes(),
            request_id.as_bytes(),
        ]);
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        DedupeKey(u64::from_be_bytes(bytes))
    }
}

/// Decides which salt this run uses, refusing to guess when the two sources
/// disagree.
pub fn reconcile(
    bucket: Option<&str>,
    configured: Option<&str>,
) -> Result<(Salt, SaltOrigin), SaltError> {
    match (bucket, configured) {
        (Some(bucket), Some(configured)) => {
            let bucket = Salt::parse(bucket)?;
            let configured = Salt::parse(configured)?;
            if bucket != configured {
                return Err(SaltError::Mismatch {
                    bucket: bucket.fingerprint(),
                    configured: configured.fingerprint(),
                });
            }
            Ok((bucket, SaltOrigin::Bucket))
        }
        (Some(bucket), None) => Ok((Salt::parse(bucket)?, SaltOrigin::Bucket)),
        (None, Some(configured)) => Ok((Salt::parse(configured)?, SaltOrigin::Configured)),
        (None, None) => Ok((Salt::mint(), SaltOrigin::Minted)),
    }
}

/// Length-prefixed so no pair of adjacent inputs can be shifted between each
/// other without changing the digest.
fn digest(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt(value: &str) -> Salt {
        Salt::parse(value).expect("a well formed salt")
    }

    const ONE: &str = "00112233445566778899aabbccddeeff";
    const OTHER: &str = "ffeeddccbbaa99887766554433221100";

    #[test]
    fn a_minted_salt_is_usable_and_unique() {
        let first = Salt::mint();
        let second = Salt::mint();

        assert_eq!(Salt::parse(first.expose()), Ok(first.clone()));
        assert_ne!(first, second);
    }

    /// The whole point: the same path under two buckets must not produce the
    /// same hash, or one leaked mapping deanonymizes every other bucket.
    #[test]
    fn the_same_project_hashes_differently_under_two_buckets() {
        let path = "/home/ubuntu/dev/acme-billing";

        assert_ne!(salt(ONE).hash_project(path), salt(OTHER).hash_project(path));
    }

    #[test]
    fn a_project_hash_never_contains_the_path() {
        let path = "/home/ubuntu/dev/acme-billing";

        let hashed = salt(ONE).hash_project(path);

        assert!(!hashed.contains("acme"), "{hashed}");
        assert!(!hashed.contains("ubuntu"), "{hashed}");
    }

    #[test]
    fn two_machines_sharing_a_salt_agree_on_project_and_dedupe_hashes() {
        let path = "/home/ubuntu/dev/acme-billing";

        assert_eq!(salt(ONE).hash_project(path), salt(ONE).hash_project(path));
        assert_eq!(
            salt(ONE).dedupe_key("msg-1", "req-1"),
            salt(ONE).dedupe_key("msg-1", "req-1")
        );
    }

    #[test]
    fn dedupe_keys_separate_their_inputs() {
        let salt = salt(ONE);

        assert_ne!(salt.dedupe_key("ab", "c"), salt.dedupe_key("a", "bc"));
    }

    /// Continuing with the wrong salt would count every shared entry twice,
    /// which is worse than refusing to sync.
    #[test]
    fn disagreeing_salts_stop_the_sync_instead_of_falling_back() {
        let error = reconcile(Some(ONE), Some(OTHER)).expect_err("a mismatch is fatal");

        assert!(matches!(error, SaltError::Mismatch { .. }));
    }

    #[test]
    fn a_second_machine_adopts_the_bucket_salt() {
        let (adopted, origin) = reconcile(Some(ONE), None).expect("adopt");

        assert_eq!(adopted, salt(ONE));
        assert_eq!(origin, SaltOrigin::Bucket);
    }

    #[test]
    fn a_first_setup_mints_one() {
        let (_, origin) = reconcile(None, None).expect("mint");

        assert_eq!(origin, SaltOrigin::Minted);
    }

    #[test]
    fn a_truncated_or_non_hex_salt_is_refused_rather_than_padded() {
        assert!(matches!(
            Salt::parse("00112233"),
            Err(SaltError::Malformed { .. })
        ));
        assert!(matches!(
            Salt::parse("00112233445566778899aabbccddeegg"),
            Err(SaltError::Malformed { .. })
        ));
    }

    /// A salt in a log or a bug report undoes every hash in the bucket.
    #[test]
    fn debug_output_does_not_contain_the_salt() {
        let rendered = format!("{:?}", salt(ONE));

        assert_eq!(rendered, "Salt(<redacted>)");
        assert!(!rendered.contains(ONE));
    }

    #[test]
    fn the_fingerprint_identifies_without_revealing() {
        let fingerprint = salt(ONE).fingerprint();

        assert_eq!(fingerprint.len(), 8);
        assert_ne!(fingerprint, salt(OTHER).fingerprint());
        assert!(!ONE.contains(&fingerprint), "{fingerprint}");
    }
}
