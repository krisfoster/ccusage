//! `ccusage sync doctor`: the checks that need the bucket.
//!
//! Three things break sync in ways a user cannot diagnose from an error at
//! upload time: no write access, a store that ignores preconditions (without
//! working compare-and-swap, two machines silently overwrite each other's
//! merges), and a clock far enough out that usage lands in the wrong 15-minute
//! bucket. Doctor provokes all three deliberately against a throwaway probe
//! object, so the checks run against `MemoryStore` in tests exactly as they run
//! against Cloud Storage.

use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition};
use serde_json::{Value, json};

use super::bucket::public_member;

/// Beyond this the 15-minute bucket a sample lands in can be the wrong one.
const SKEW_WARN_MS: i64 = 60_000;
/// Beyond this, merges across machines interleave incorrectly and dedupe by
/// bucket stops holding.
const SKEW_FAIL_MS: i64 = 300_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Pass,
    Warn,
    Fail,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) outcome: Outcome,
    pub(crate) detail: String,
}

impl Check {
    fn new(name: &'static str, outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            name,
            outcome,
            detail: detail.into(),
        }
    }
}

pub(crate) fn to_json(checks: &[Check]) -> Value {
    json!({
        "ok": checks.iter().all(|check| check.outcome != Outcome::Fail),
        "checks": checks
            .iter()
            .map(|check| json!({
                "name": check.name,
                "outcome": check.outcome.as_str(),
                "detail": check.detail,
            }))
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn to_text(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|check| {
            format!(
                "[{}] {} — {}",
                check.outcome.as_str(),
                check.name,
                check.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the bucket itself is configured to keep usage data private.
pub(crate) fn privacy_check(uniform_bucket_level_access: bool, policy: &Value) -> Check {
    if !uniform_bucket_level_access {
        return Check::new(
            "bucket privacy",
            Outcome::Fail,
            "uniform bucket-level access is off, so an object ACL could expose usage data",
        );
    }
    match public_member(policy) {
        Some(member) => Check::new(
            "bucket privacy",
            Outcome::Fail,
            format!("'{member}' can read the whole bucket, including your usage data"),
        ),
        None => Check::new(
            "bucket privacy",
            Outcome::Pass,
            "only the dashboard prefix is world-readable",
        ),
    }
}

/// Writes, re-reads, provokes a precondition conflict, and cleans up.
///
/// The probe object is machine-scoped and deleted at the end; a leftover from a
/// crashed run is overwritten rather than treated as a conflict, so doctor never
/// needs manual cleanup to pass twice.
pub(crate) fn run_checks(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    machine_id: &str,
    now_ms: i64,
) -> Vec<Check> {
    let key = match keys.probe(machine_id) {
        Ok(key) => key,
        Err(error) => {
            return vec![Check::new(
                "write access",
                Outcome::Fail,
                format!("could not build a probe key: {error}"),
            )];
        }
    };
    let body = b"ccusage doctor probe";

    let meta = match store.put(&key, body, "application/octet-stream", &Precondition::None) {
        Ok(meta) => meta,
        Err(error) => {
            return vec![Check::new(
                "write access",
                Outcome::Fail,
                format!("cannot write to the bucket: {error}"),
            )];
        }
    };
    let mut checks = vec![Check::new(
        "write access",
        Outcome::Pass,
        "wrote and can delete a probe object",
    )];
    checks.push(read_back_check(store, &key, body));
    checks.push(cas_check(store, &key, &meta.generation));
    checks.push(skew_check(meta.updated_ms, now_ms));

    if let Err(error) = store.delete(&key, &Precondition::None) {
        checks.push(Check::new(
            "probe cleanup",
            Outcome::Warn,
            format!("the probe object could not be deleted: {error}"),
        ));
    }
    checks
}

fn read_back_check(store: &dyn ObjectStore, key: &Key, expected: &[u8]) -> Check {
    match store.get(key) {
        Ok(Some((body, _))) if body == expected => Check::new(
            "read back",
            Outcome::Pass,
            "the probe object read back byte for byte",
        ),
        Ok(Some(_)) => Check::new(
            "read back",
            Outcome::Fail,
            "the probe object read back with different contents",
        ),
        Ok(None) => Check::new(
            "read back",
            Outcome::Fail,
            "the probe object disappeared immediately after being written",
        ),
        Err(error) => Check::new(
            "read back",
            Outcome::Fail,
            format!("the probe object could not be read: {error}"),
        ),
    }
}

/// A store that accepts a write whose generation precondition is stale would let
/// one machine's merge silently erase another's, so this check failing is a
/// refusal to sync rather than a warning.
fn cas_check(store: &dyn ObjectStore, key: &Key, generation: &Option<String>) -> Check {
    let Some(generation) = generation else {
        return Check::new(
            "compare-and-swap",
            Outcome::Fail,
            "the store did not report an object generation, so concurrent merges cannot be \
             made safe",
        );
    };
    // A generation the object provably does not have: appending a digit to the
    // live one stays a well-formed generation without guessing a free value.
    let stale = Precondition::IfGenerationMatch(format!("{generation}0"));
    match store.put(key, b"stale", "application/octet-stream", &stale) {
        Err(ObjectStoreError::Conflict { .. }) => Check::new(
            "compare-and-swap",
            Outcome::Pass,
            "a write against a stale generation was rejected",
        ),
        Ok(_) => Check::new(
            "compare-and-swap",
            Outcome::Fail,
            "a write against a stale generation was accepted, so concurrent machines would \
             overwrite each other",
        ),
        Err(error) => Check::new(
            "compare-and-swap",
            Outcome::Fail,
            format!("the precondition could not be tested: {error}"),
        ),
    }
}

fn skew_check(server_ms: Option<i64>, now_ms: i64) -> Check {
    let Some(server_ms) = server_ms else {
        return Check::new(
            "clock skew",
            Outcome::Warn,
            "the store did not report a modification time, so clock skew cannot be measured",
        );
    };
    let skew = (server_ms - now_ms).abs();
    let detail = format!(
        "this machine is {:.1}s from the bucket's clock",
        skew as f64 / 1000.0
    );
    let outcome = if skew >= SKEW_FAIL_MS {
        Outcome::Fail
    } else if skew >= SKEW_WARN_MS {
        Outcome::Warn
    } else {
        Outcome::Pass
    };
    Check::new("clock skew", outcome, detail)
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::objectstore::{Fault, MemoryStore};

    use super::*;

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    fn outcome(checks: &[Check], name: &str) -> Outcome {
        checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("missing check {name}: {checks:?}"))
            .outcome
    }

    #[test]
    fn a_healthy_store_passes_every_check_and_leaves_nothing_behind() {
        let store = MemoryStore::new();
        let keys = keys();

        let checks = run_checks(&store, &keys, "machine-1", store.now_ms());

        assert!(
            checks.iter().all(|check| check.outcome == Outcome::Pass),
            "{checks:?}"
        );
        assert_eq!(
            store
                .get(&keys.probe("machine-1").expect("key"))
                .expect("get"),
            None
        );
    }

    #[test]
    fn a_bucket_that_cannot_be_written_to_reports_nothing_else() {
        let store = MemoryStore::new();
        store.fail_next(Fault::Network);

        let checks = run_checks(&store, &keys(), "machine-1", 0);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].outcome, Outcome::Fail);
    }

    #[test]
    fn a_clock_an_hour_out_fails_because_usage_would_land_in_the_wrong_bucket() {
        let store = MemoryStore::new();

        let checks = run_checks(&store, &keys(), "machine-1", store.now_ms() - 3_600_000);

        assert_eq!(outcome(&checks, "clock skew"), Outcome::Fail);
    }

    #[test]
    fn a_clock_two_minutes_out_only_warns() {
        let store = MemoryStore::new();

        let checks = run_checks(&store, &keys(), "machine-1", store.now_ms() - 120_000);

        assert_eq!(outcome(&checks, "clock skew"), Outcome::Warn);
    }

    #[test]
    fn a_public_bucket_fails_the_privacy_check() {
        let policy = json!({
            "bindings": [{ "role": "roles/storage.objectViewer", "members": ["allUsers"] }]
        });

        assert_eq!(privacy_check(true, &policy).outcome, Outcome::Fail);
    }

    #[test]
    fn object_acls_fail_the_privacy_check_even_with_no_public_binding() {
        assert_eq!(privacy_check(false, &json!({})).outcome, Outcome::Fail);
    }

    #[test]
    fn the_json_report_says_whether_anything_failed() {
        let checks = vec![
            Check::new("write access", Outcome::Pass, "ok"),
            Check::new("clock skew", Outcome::Warn, "close enough"),
        ];

        assert_eq!(to_json(&checks)["ok"], json!(true));
    }
}
