//! Deciding which bucket setup should use, and proving the one it ends up with
//! is private.
//!
//! Naming is the delicate part. A bucket name is globally unique across all of
//! Google Cloud, so a guessable name is both likely to be taken and, once taken
//! by someone else, an invitation to probe it. Setup therefore mints a random
//! name rather than deriving one from the user or machine, and once a name is in
//! the config it is never silently replaced: pointing at a different bucket
//! abandons the history in the old one, which is a choice the user makes with
//! `--recreate`, not one a flag typo makes for them.

use ccusage_objectstore::{ObjectStoreError, Result};
use serde_json::Value;

use crate::gcs::bucket::{BucketAdmin, BucketInfo, BucketSpec};

/// Bytes of randomness in a minted name. 48 bits is short enough to read out
/// over a call and far beyond anything worth enumerating at bucket-creation
/// rates.
const MINTED_BYTES: usize = 6;
const NAME_PREFIX: &str = "ccusage-";
const MIN_NAME_LENGTH: usize = 3;
const MAX_NAME_LENGTH: usize = 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BucketOrigin {
    /// Named on the command line, and different from what the config holds.
    Requested,
    /// Already in the config: setup is being re-run.
    Configured,
    /// Newly minted for a first-time setup.
    Minted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedBucket {
    pub(crate) name: String,
    pub(crate) origin: BucketOrigin,
}

pub(crate) fn plan(
    requested: Option<&str>,
    configured: Option<&str>,
    recreate: bool,
    entropy: &mut dyn FnMut() -> [u8; MINTED_BYTES],
) -> Result<PlannedBucket> {
    let configured = configured.map(str::trim).filter(|name| !name.is_empty());
    let requested = match requested.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => Some(normalize(name)?),
        None => None,
    };

    match (requested, configured) {
        (Some(requested), Some(configured)) if requested != configured && !recreate => {
            Err(ObjectStoreError::Other {
                detail: format!(
                    "this machine already syncs to bucket '{configured}', and '{requested}' is a \
                     different bucket. Usage already uploaded to '{configured}' would not move. \
                     Re-run with --recreate to switch to '{requested}', or drop --bucket to keep \
                     using '{configured}'"
                ),
            })
        }
        (Some(requested), Some(configured)) if requested == configured => Ok(PlannedBucket {
            name: requested,
            origin: BucketOrigin::Configured,
        }),
        (Some(requested), _) => Ok(PlannedBucket {
            name: requested,
            origin: BucketOrigin::Requested,
        }),
        (None, Some(configured)) => Ok(PlannedBucket {
            name: normalize(configured)?,
            origin: BucketOrigin::Configured,
        }),
        (None, None) => Ok(PlannedBucket {
            name: mint(entropy()),
            origin: BucketOrigin::Minted,
        }),
    }
}

/// Accepts what a user is likely to paste — a `gs://` URL, a trailing slash,
/// stray case — and rejects anything Cloud Storage itself would reject, because
/// its own error for a bad name arrives as a flat 400.
fn normalize(name: &str) -> Result<String> {
    let candidate = name
        .trim()
        .trim_start_matches("gs://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let invalid = |reason: &str| ObjectStoreError::InvalidKey {
        key: candidate.clone(),
        reason: reason.to_string(),
    };

    if !(MIN_NAME_LENGTH..=MAX_NAME_LENGTH).contains(&candidate.len()) {
        return Err(invalid("bucket names are 3 to 63 characters long"));
    }
    if !candidate
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte))
    {
        return Err(invalid(
            "bucket names may only contain lowercase letters, digits, '-', '_' and '.'",
        ));
    }
    let edges_are_alphanumeric = |text: &str| {
        text.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && text
                .bytes()
                .next_back()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    };
    if !edges_are_alphanumeric(&candidate) {
        return Err(invalid(
            "bucket names must start and end with a letter or digit",
        ));
    }
    if candidate.starts_with("goog") || candidate.contains("google") {
        return Err(invalid("bucket names may not reference Google"));
    }
    Ok(candidate)
}

fn mint(bytes: [u8; MINTED_BYTES]) -> String {
    let mut name = String::with_capacity(NAME_PREFIX.len() + MINTED_BYTES * 2);
    name.push_str(NAME_PREFIX);
    for byte in bytes {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

pub(crate) fn os_entropy() -> [u8; MINTED_BYTES] {
    let mut bytes = [0_u8; MINTED_BYTES];
    getrandom::fill(&mut bytes).expect("the operating system must provide randomness");
    bytes
}

/// Creates the bucket if needed, then refuses to continue unless it is private.
///
/// The check is not ceremony: `ensure` adopts a bucket that already exists, and
/// an adopted bucket may be one the user previously made public. Uploading usage
/// into it would publish their spend, so the failure has to happen before the
/// first object is written.
pub(crate) fn ensure_private(admin: &BucketAdmin, spec: &BucketSpec) -> Result<BucketInfo> {
    let info = admin.ensure(spec)?;
    if !info.uniform_bucket_level_access {
        return Err(ObjectStoreError::Other {
            detail: format!(
                "bucket '{}' does not use uniform bucket-level access, so object ACLs could \
                 expose usage data. Enable it on the bucket and re-run setup",
                info.name
            ),
        });
    }
    let policy = admin.get_iam_policy()?;
    if let Some(member) = public_member(&policy) {
        return Err(ObjectStoreError::Other {
            detail: format!(
                "bucket '{}' grants '{member}' read access to the whole bucket, which would \
                 publish your usage data. Remove that binding and re-run setup",
                info.name
            ),
        });
    }
    Ok(info)
}

/// An unconditional `allUsers`/`allAuthenticatedUsers` binding. Conditional ones
/// are left alone: the dashboard's own grant is conditional on the public prefix,
/// and re-running setup must not trip over it.
pub(crate) fn public_member(policy: &Value) -> Option<String> {
    policy
        .get("bindings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|binding| binding.get("condition").is_none())
        .filter_map(|binding| binding.get("members").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .find(|member| matches!(*member, "allUsers" | "allAuthenticatedUsers"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixed_entropy() -> impl FnMut() -> [u8; MINTED_BYTES] {
        || [0x9f, 0x3a, 0x1c, 0x2b, 0x44, 0x05]
    }

    #[test]
    fn mints_an_unguessable_name_on_a_first_setup() {
        let planned = plan(None, None, false, &mut fixed_entropy()).expect("plan");

        assert_eq!(
            planned,
            PlannedBucket {
                name: "ccusage-9f3a1c2b4405".to_string(),
                origin: BucketOrigin::Minted,
            }
        );
    }

    #[test]
    fn re_running_setup_keeps_the_configured_bucket() {
        let planned = plan(None, Some("ccusage-existing"), false, &mut fixed_entropy())
            .expect("a second run must not mint a second bucket");

        assert_eq!(planned.name, "ccusage-existing");
        assert_eq!(planned.origin, BucketOrigin::Configured);
    }

    #[test]
    fn a_different_bucket_needs_recreate_because_history_would_be_stranded() {
        let error = plan(
            Some("ccusage-other"),
            Some("ccusage-existing"),
            false,
            &mut fixed_entropy(),
        )
        .expect_err("switching buckets silently would hide the old data");

        let message = error.to_string();
        assert!(message.contains("ccusage-existing"), "{message}");
        assert!(message.contains("--recreate"), "{message}");
    }

    #[test]
    fn recreate_switches_to_the_requested_bucket() {
        let planned = plan(
            Some("ccusage-other"),
            Some("ccusage-existing"),
            true,
            &mut fixed_entropy(),
        )
        .expect("plan");

        assert_eq!(planned.name, "ccusage-other");
        assert_eq!(planned.origin, BucketOrigin::Requested);
    }

    #[test]
    fn naming_the_configured_bucket_again_is_not_a_switch() {
        let planned = plan(
            Some("gs://ccusage-existing/"),
            Some("ccusage-existing"),
            false,
            &mut fixed_entropy(),
        )
        .expect("the same bucket spelled as a URL is the same bucket");

        assert_eq!(planned.origin, BucketOrigin::Configured);
    }

    #[test]
    fn rejects_names_cloud_storage_would_reject() {
        for name in ["ab", "Ccusage Bucket", "-leading", "google-usage"] {
            let error = plan(Some(name), None, false, &mut fixed_entropy())
                .expect_err("invalid name should not reach the API");
            assert!(
                matches!(error, ObjectStoreError::InvalidKey { .. }),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn an_unconditional_public_binding_is_found() {
        let policy = json!({
            "bindings": [
                { "role": "roles/storage.objectViewer", "members": ["allUsers"] },
            ]
        });

        assert_eq!(public_member(&policy).as_deref(), Some("allUsers"));
    }

    #[test]
    fn the_dashboards_own_conditional_grant_is_not_a_leak() {
        let policy = json!({
            "bindings": [
                {
                    "role": "roles/storage.objectViewer",
                    "members": ["allUsers"],
                    "condition": { "expression": "resource.name.startsWith('x')" },
                },
                { "role": "roles/storage.admin", "members": ["user:me@example.com"] },
            ]
        });

        assert_eq!(public_member(&policy), None);
    }
}
