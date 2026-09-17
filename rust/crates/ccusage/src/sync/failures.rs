//! What a sync says when the bucket, the network, or the clock lets it down.
//!
//! Every one of these failures is recoverable by re-running: shards are
//! written before the index that names them, the index is written before the
//! rollups derived from it, and every one of those writes is idempotent. So
//! the job here is not recovery, it is telling the user which of the three
//! things went wrong and what to do about it, because "permission denied" on
//! its own sends people to the wrong place.

use ccusage_objectstore::ObjectStoreError;

/// How far apart the two clocks have to be before a sync is refused.
///
/// Usage is bucketed into 15-minute cells by this machine's clock, so a skew
/// under one cell can only move an entry between adjacent buckets. Beyond an
/// hour, days land under the wrong date and the dedupe keys of two machines
/// stop lining up — which is silent, and worse than stopping.
pub(crate) const SKEW_REFUSE_MS: i64 = 3_600_000;

/// Turns a store error into something the user can act on.
pub(crate) fn explain(error: &ObjectStoreError) -> String {
    match error {
        ObjectStoreError::Unauthenticated { source, detail } => format!(
            "the credentials this sync was using stopped working part-way through ({source}: {detail}). Nothing was lost — re-authenticate with 'gcloud auth application-default login' and run 'ccusage sync' again to pick up where it stopped."
        ),
        ObjectStoreError::Forbidden { key, detail } => format!(
            "this account may not write {key} ({detail}). Grant it 'roles/storage.objectAdmin' on the bucket, or run 'ccusage sync doctor' to see which checks fail."
        ),
        ObjectStoreError::Network { detail } => format!(
            "the bucket could not be reached ({detail}). Whatever had already been uploaded is still there; re-run 'ccusage sync' when the connection is back."
        ),
        ObjectStoreError::RateLimited { .. } | ObjectStoreError::Server { .. } => format!(
            "the storage service is not accepting writes right now ({error}). Re-run 'ccusage sync' in a few minutes; a partly finished sync resumes without duplicating anything."
        ),
        ObjectStoreError::Conflict { key } => format!(
            "another sync kept changing {key} while this one was writing it. Re-run 'ccusage sync' once no other machine is syncing."
        ),
        other => other.to_string(),
    }
}

/// Refuses a sync whose clock disagrees with the bucket's by more than an
/// hour, naming the direction so the user knows which clock to look at.
pub(crate) fn check_clock(server_ms: Option<i64>, now_ms: i64) -> Result<(), String> {
    let Some(server_ms) = server_ms else {
        return Ok(());
    };
    let skew = now_ms - server_ms;
    if skew.abs() < SKEW_REFUSE_MS {
        return Ok(());
    }
    let direction = if skew > 0 { "ahead of" } else { "behind" };
    Err(format!(
        "this machine's clock is {:.0} minutes {direction} the bucket's, so usage would be filed under the wrong days. Fix the system clock and run 'ccusage sync' again.",
        (skew.abs() as f64) / 60_000.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials that expire mid-run are the common case for a long sync,
    /// and the one where the user most needs to be told that re-running is
    /// safe rather than starting over by hand.
    #[test]
    fn an_expired_credential_names_the_source_and_says_a_re_run_resumes() {
        let message = explain(&ObjectStoreError::Unauthenticated {
            source: "ADC".to_string(),
            detail: "token expired".to_string(),
        });

        assert!(message.contains("ADC"), "{message}");
        assert!(
            message.contains("gcloud auth application-default login"),
            "{message}"
        );
        assert!(message.contains("Nothing was lost"), "{message}");
    }

    #[test]
    fn being_offline_says_what_survived_and_what_to_do() {
        let message = explain(&ObjectStoreError::Network {
            detail: "dns failure".to_string(),
        });

        assert!(message.contains("dns failure"), "{message}");
        assert!(message.contains("re-run"), "{message}");
    }

    #[test]
    fn a_permission_failure_names_the_object_and_the_role_to_grant() {
        let message = explain(&ObjectStoreError::Forbidden {
            key: "ccusage/v1/manifest.json".to_string(),
            detail: "missing storage.objects.create".to_string(),
        });

        assert!(message.contains("ccusage/v1/manifest.json"), "{message}");
        assert!(message.contains("roles/storage.objectAdmin"), "{message}");
    }

    #[test]
    fn a_clock_within_a_quarter_hour_is_allowed_through() {
        assert!(check_clock(Some(1_000_000_000_000), 1_000_000_600_000).is_ok());
    }

    #[test]
    fn a_clock_hours_out_refuses_the_sync_and_says_which_way() {
        let ahead =
            check_clock(Some(1_000_000_000_000), 1_000_010_800_000).expect_err("three hours ahead");
        let behind = check_clock(Some(1_000_010_800_000), 1_000_000_000_000)
            .expect_err("three hours behind");

        assert!(ahead.contains("180 minutes ahead of"), "{ahead}");
        assert!(behind.contains("180 minutes behind"), "{behind}");
    }

    /// A store that reports no modification time is not evidence of a bad
    /// clock, and refusing to sync on missing evidence would strand anyone
    /// whose provider omits it.
    #[test]
    fn a_store_that_reports_no_time_does_not_block_the_sync() {
        assert!(check_clock(None, 1_000_000_000_000).is_ok());
    }
}
