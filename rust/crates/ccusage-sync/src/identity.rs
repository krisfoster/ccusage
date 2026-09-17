//! Resolution of the two identifiers every synced object is filed under.
//!
//! Per the design's DR-03 and DR-04 neither is derived from the machine's hardware
//! or from a provider credential file: the machine ID is minted once and kept in
//! local config, and the user ID comes from the bucket the user authenticated to,
//! so a second machine joins an existing dataset by pointing at the same bucket.

use std::fmt;

use sha2::{Digest, Sha256};

/// Minted identifiers are 128 bits of OS entropy rendered as hex. Long enough that
/// two machines never collide, short enough to read back over a support channel.
const MINTED_BYTES: usize = 16;

const MAX_IDENTIFIER_LENGTH: usize = 64;

/// The salted hash that reaches an object key. Truncation keeps keys short; 48 bits
/// is ample when the input is already 128 bits of entropy and the salt is per-user.
const HASHED_IDENTIFIER_HEX_LENGTH: usize = 12;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserId(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineId(String);

impl UserId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MachineId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a resolved identifier came from, so `sync status` can say whether it is
/// pinned in config, adopted from the bucket, or brand new.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityOrigin {
    Configured,
    Manifest,
    Minted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentityWarning {
    /// The machine ID was configured on a machine with a different fingerprint,
    /// which is what a copied dotfile looks like. Two machines sharing an ID
    /// interleave writes into one shard, so this is worth interrupting for.
    ConfigLooksCopied {
        recorded_fingerprint: String,
        observed_fingerprint: String,
    },
}

impl fmt::Display for IdentityWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigLooksCopied { .. } => formatter.write_str(
                "sync.machineId was set up on a different machine, which usually means the config was copied. Run 'ccusage sync setup --recreate' to mint a new machine ID, or keep this one if you restored a backup of the same machine.",
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentityError {
    InvalidUserId(String),
    InvalidMachineId(String),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (field, value) = match self {
            Self::InvalidUserId(value) => ("sync.userId", value),
            Self::InvalidMachineId(value) => ("sync.machineId", value),
        };
        write!(
            formatter,
            "{field} '{value}' is not usable as an object key segment. Expected 1-{MAX_IDENTIFIER_LENGTH} characters from a-z, 0-9, '-' and '_'"
        )
    }
}

impl std::error::Error for IdentityError {}

#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityInputs<'a> {
    /// `sync.userId`, set by a user who wants to pin the value.
    pub configured_user_id: Option<&'a str>,
    /// The user ID already recorded in the configured bucket's manifest.
    pub manifest_user_id: Option<&'a str>,
    /// `sync.machineId`, written back by the first successful setup.
    pub configured_machine_id: Option<&'a str>,
    /// The fingerprint observed when the machine ID was minted.
    pub recorded_fingerprint: Option<&'a str>,
    /// The fingerprint observed now. `None` on platforms with no readable source,
    /// which only disables the copied-config check.
    pub observed_fingerprint: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedIdentity {
    pub user: UserId,
    pub user_origin: IdentityOrigin,
    pub machine: MachineId,
    pub machine_origin: IdentityOrigin,
    pub warnings: Vec<IdentityWarning>,
}

/// `entropy` is injected so tests are deterministic and so the caller owns the
/// failure mode of a missing entropy source.
pub fn resolve_identity(
    inputs: &IdentityInputs<'_>,
    entropy: &mut dyn FnMut() -> [u8; MINTED_BYTES],
) -> Result<ResolvedIdentity, IdentityError> {
    let (user, user_origin) = match (inputs.configured_user_id, inputs.manifest_user_id) {
        (Some(configured), _) => (
            validate(configured)
                .ok_or_else(|| IdentityError::InvalidUserId(configured.to_string()))?,
            IdentityOrigin::Configured,
        ),
        (None, Some(manifest)) => (
            validate(manifest).ok_or_else(|| IdentityError::InvalidUserId(manifest.to_string()))?,
            IdentityOrigin::Manifest,
        ),
        (None, None) => (mint(entropy), IdentityOrigin::Minted),
    };
    let (machine, machine_origin) = match inputs.configured_machine_id {
        Some(configured) => (
            validate(configured)
                .ok_or_else(|| IdentityError::InvalidMachineId(configured.to_string()))?,
            IdentityOrigin::Configured,
        ),
        None => (mint(entropy), IdentityOrigin::Minted),
    };
    Ok(ResolvedIdentity {
        user: UserId(user),
        user_origin,
        machine: MachineId(machine),
        machine_origin,
        warnings: copied_config_warning(inputs).into_iter().collect(),
    })
}

/// The value that reaches an object key. Salted because the inputs are stable
/// across every object a user writes, and an unsalted hash of a short identifier
/// is one dictionary away from the identifier itself.
pub fn hash_identifier(salt: &[u8], value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(HASHED_IDENTIFIER_HEX_LENGTH)
        .collect()
}

/// A freshly minted machine ID cannot have been copied, so the check only applies
/// to a configured one.
fn copied_config_warning(inputs: &IdentityInputs<'_>) -> Option<IdentityWarning> {
    let recorded = inputs.recorded_fingerprint?;
    let observed = inputs.observed_fingerprint?;
    if inputs.configured_machine_id.is_none() || recorded == observed {
        return None;
    }
    Some(IdentityWarning::ConfigLooksCopied {
        recorded_fingerprint: recorded.to_string(),
        observed_fingerprint: observed.to_string(),
    })
}

fn mint(entropy: &mut dyn FnMut() -> [u8; MINTED_BYTES]) -> String {
    entropy().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate(value: &str) -> Option<String> {
    let usable = (1..=MAX_IDENTIFIER_LENGTH).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    usable.then(|| value.to_string())
}

/// 128 bits from the OS. Panics only if the OS entropy source is unavailable, which
/// on every supported platform means the process cannot do anything useful anyway.
pub fn os_entropy() -> [u8; MINTED_BYTES] {
    let mut bytes = [0u8; MINTED_BYTES];
    getrandom::fill(&mut bytes).expect("the OS entropy source is unavailable");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_both_identifiers_for_a_first_time_setup() {
        let identity = resolve(&IdentityInputs::default());

        assert_eq!(identity.user_origin, IdentityOrigin::Minted);
        assert_eq!(identity.machine_origin, IdentityOrigin::Minted);
        assert_eq!(identity.user.as_str().len(), MINTED_BYTES * 2);
        assert_ne!(identity.user.as_str(), identity.machine.as_str());
        assert!(identity.warnings.is_empty());
    }

    #[test]
    fn adopts_the_user_id_the_bucket_already_holds() {
        let identity = resolve(&IdentityInputs {
            manifest_user_id: Some("7q4dexisting"),
            ..IdentityInputs::default()
        });

        assert_eq!(identity.user.as_str(), "7q4dexisting");
        assert_eq!(identity.user_origin, IdentityOrigin::Manifest);
        assert_eq!(identity.machine_origin, IdentityOrigin::Minted);
    }

    #[test]
    fn a_configured_user_id_outranks_the_manifest() {
        let identity = resolve(&IdentityInputs {
            configured_user_id: Some("pinned"),
            manifest_user_id: Some("7q4dexisting"),
            ..IdentityInputs::default()
        });

        assert_eq!(identity.user.as_str(), "pinned");
        assert_eq!(identity.user_origin, IdentityOrigin::Configured);
    }

    #[test]
    fn keeps_the_configured_machine_id_across_runs() {
        let identity = resolve(&IdentityInputs {
            configured_machine_id: Some("9f3a1c2b"),
            ..IdentityInputs::default()
        });

        assert_eq!(identity.machine.as_str(), "9f3a1c2b");
        assert_eq!(identity.machine_origin, IdentityOrigin::Configured);
    }

    #[test]
    fn warns_when_a_configured_machine_id_arrives_on_another_machine() {
        let identity = resolve(&IdentityInputs {
            configured_machine_id: Some("9f3a1c2b"),
            recorded_fingerprint: Some("aaaa"),
            observed_fingerprint: Some("bbbb"),
            ..IdentityInputs::default()
        });

        assert_eq!(
            identity.warnings,
            vec![IdentityWarning::ConfigLooksCopied {
                recorded_fingerprint: "aaaa".to_string(),
                observed_fingerprint: "bbbb".to_string(),
            }]
        );
    }

    #[test]
    fn does_not_warn_without_a_fingerprint_to_compare() {
        let identity = resolve(&IdentityInputs {
            configured_machine_id: Some("9f3a1c2b"),
            recorded_fingerprint: Some("aaaa"),
            observed_fingerprint: None,
            ..IdentityInputs::default()
        });

        assert!(identity.warnings.is_empty());
    }

    #[test]
    fn rejects_identifiers_that_would_escape_their_key_segment() {
        for value in ["../elsewhere", "has/slash", "UPPER", "", &"x".repeat(65)] {
            let error = resolve_identity(
                &IdentityInputs {
                    configured_machine_id: Some(value),
                    ..IdentityInputs::default()
                },
                &mut counting_entropy(),
            )
            .expect_err("expected an invalid machine ID to be rejected");

            assert_eq!(error, IdentityError::InvalidMachineId(value.to_string()));
            assert!(error.to_string().contains("sync.machineId"));
        }
    }

    #[test]
    fn hashing_hides_the_identifier_and_depends_on_the_salt() {
        let hashed = hash_identifier(b"salt-one", "9f3a1c2b");

        assert_eq!(hashed.len(), HASHED_IDENTIFIER_HEX_LENGTH);
        assert!(!hashed.contains("9f3a1c2b"));
        assert_ne!(hashed, hash_identifier(b"salt-two", "9f3a1c2b"));
        assert_eq!(hashed, hash_identifier(b"salt-one", "9f3a1c2b"));
    }

    fn resolve(inputs: &IdentityInputs<'_>) -> ResolvedIdentity {
        resolve_identity(inputs, &mut counting_entropy()).expect("valid identity inputs")
    }

    /// Distinct per call, so a test can tell a minted user ID from a minted machine ID.
    fn counting_entropy() -> impl FnMut() -> [u8; MINTED_BYTES] {
        let mut counter = 0u8;
        move || {
            counter += 1;
            [counter; MINTED_BYTES]
        }
    }
}
