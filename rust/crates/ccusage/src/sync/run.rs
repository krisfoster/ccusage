//! `ccusage sync run`: fold this machine's local usage into shards and upload
//! the ones the bucket does not already have.
//!
//! The upload is incremental by content hash rather than by timestamp. A shard
//! whose hash matches the machine index is not re-uploaded, so a sync that
//! finds nothing new costs one read of the index and nothing else, and a day
//! that gains a single entry rewrites only that day.

use ccusage_config::ConfigContext;
use ccusage_core::LoadedEntry;
use ccusage_objectstore::{KeySpace, ObjectStore, Precondition, UtcDate};
use ccusage_sync::{FoldContext, FoldEntry, Salt, Shard, fold};
use std::sync::Arc;

use super::machine::{self, IndexEntry, MachineIndex};
use super::{DEFAULT_PREFIX, config_auth_mode, now_ms};
use crate::{
    Result,
    cli::{SharedArgs, SyncRunArgs},
    cli_error, format_rfc3339_millis,
    gcs::GcsStore,
    load_entries,
    sync::{auth, rollups, status},
};

/// The agent this build syncs. Shards are keyed by agent, so adding another is
/// a matter of folding its entries under a different name, not a layout change.
const AGENT: &str = "claude";

/// What a run did, in the terms the user is told about.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunSummary {
    pub uploaded: Vec<String>,
    pub unchanged: usize,
}

impl RunSummary {
    pub fn to_text(&self, dry_run: bool) -> String {
        if self.uploaded.is_empty() {
            return format!("Nothing to sync; {} days already current.", self.unchanged);
        }
        let verb = if dry_run { "Would upload" } else { "Uploaded" };
        format!(
            "{verb} {} day(s): {}. {} already current.",
            self.uploaded.len(),
            self.uploaded.join(", "),
            self.unchanged
        )
    }
}

/// Splits folded shards into the ones the bucket needs and the ones it has.
///
/// Both sides are driven by the shard's own content hash, which excludes the
/// generation timestamp, so re-running sync on unchanged logs uploads nothing.
pub(crate) fn plan_uploads(
    shards: Vec<Shard>,
    index: &MachineIndex,
    agent: &str,
) -> (Vec<Shard>, usize) {
    let mut uploads = Vec::new();
    let mut unchanged = 0;
    for mut shard in shards {
        let Ok(hash) = shard.finish().map(str::to_string) else {
            continue;
        };
        if index.is_current(agent, &shard.utc_date, &hash) {
            unchanged += 1;
        } else {
            uploads.push(shard);
        }
    }
    (uploads, unchanged)
}

/// Uploads the planned shards and records them in the machine index.
///
/// The index is written after the shards, never before: a crash in between
/// leaves a shard the index does not mention, which the next run simply
/// re-uploads. The reverse order would leave the index promising a shard that
/// is not there, and rollups would read a hole as missing usage.
pub(crate) fn upload(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    shards: &[Shard],
    now: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut written = Vec::new();
    for shard in shards {
        let date = parse_date(&shard.utc_date)?;
        let key = keys
            .shard(user_id, machine_id, &shard.agent, date)
            .map_err(|error| error.to_string())?;
        let body = shard.to_json().map_err(|error| error.to_string())?;
        store
            .put(&key, &body, "application/json", &Precondition::None)
            .map_err(|error| error.to_string())?;
        written.push(shard.utc_date.clone());
    }
    if written.is_empty() {
        return Ok(written);
    }
    machine::update_index(store, keys, user_id, machine_id, |index| {
        for shard in shards {
            let Some(hash) = shard.content_hash.clone() else {
                continue;
            };
            index.shards.insert(
                MachineIndex::entry_key(&shard.agent, &shard.utc_date),
                IndexEntry {
                    content_hash: hash,
                    finalized: false,
                    updated_at: now.to_string(),
                },
            );
        }
    })?;
    Ok(written)
}

pub(crate) fn parse_date(utc_date: &str) -> std::result::Result<UtcDate, String> {
    let mut parts = utc_date.split('-');
    let invalid = || format!("'{utc_date}' is not a YYYY-MM-DD date");
    let year = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(invalid)?;
    let month = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(invalid)?;
    let day = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(invalid)?;
    UtcDate::new(year, month, day).map_err(|error| error.to_string())
}

/// Entries as the adapters produce them, reduced to what a shard may hold.
fn to_fold_entries(entries: &[LoadedEntry]) -> Vec<FoldEntry> {
    entries
        .iter()
        .map(|entry| FoldEntry {
            timestamp_ms: entry.timestamp.as_millis(),
            model: entry.model.clone().unwrap_or_else(|| "unknown".to_string()),
            input_tokens: entry.data.message.usage.input_tokens,
            output_tokens: entry.data.message.usage.output_tokens,
            cache_write_tokens: entry.data.message.usage.cache_creation_input_tokens,
            cache_read_tokens: entry.data.message.usage.cache_read_input_tokens,
            cost: entry.cost,
            session_id: entry.session_id.to_string(),
            project_path: entry.project_path.to_string(),
            message_id: entry.data.message.id.clone(),
            request_id: entry.data.request_id.clone(),
        })
        .collect()
}

pub(crate) fn execute(config: &ConfigContext, args: &SyncRunArgs) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    let (Some(bucket), Some(machine_id), Some(user_id)) = (
        status.bucket.clone(),
        status.machine_id.clone(),
        status.user_id.clone(),
    ) else {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    };
    let salt = configured_salt(config)?;
    let keys = KeySpace::new(status.prefix.as_str().trim_end_matches('/'))
        .or_else(|_| KeySpace::new(DEFAULT_PREFIX))
        .map_err(|error| cli_error(error.to_string()))?;

    let entries = load_entries(&SharedArgs::with_defaults(), None)?;
    let context = FoldContext {
        agent: AGENT,
        machine_id: &machine_id,
        user_id: &user_id,
        ccusage_version: env!("CARGO_PKG_VERSION"),
        cost_mode: "auto",
        pricing_snapshot: concat!("embedded@", env!("CARGO_PKG_VERSION")),
        generated_at: &iso_now(),
        salt: &salt,
        redact_projects: config
            .sync()
            .and_then(|sync| sync.redact_projects)
            .unwrap_or(true),
    };
    let shards = fold(&to_fold_entries(&entries), &context);

    let credentials = Arc::new(
        auth::resolve(config_auth_mode(config), true, &mut auth::TerminalPrompt)
            .map_err(|error| cli_error(error.to_string()))?,
    );
    let store = GcsStore::new(&bucket, Box::new(Arc::clone(&credentials)));
    let index = machine::load_index(&store, &keys, &user_id, &machine_id).map_err(cli_error)?;
    let (uploads, unchanged) = plan_uploads(shards, &index, AGENT);

    let summary = if args.dry_run {
        RunSummary {
            uploaded: uploads.iter().map(|shard| shard.utc_date.clone()).collect(),
            unchanged,
        }
    } else {
        let now = iso_now();
        let uploaded =
            upload(&store, &keys, &user_id, &machine_id, &uploads, &now).map_err(cli_error)?;
        machine::update_machine(&store, &keys, &user_id, &machine_id, |record| {
            record.last_sync_at = Some(now.clone());
        })
        .map_err(cli_error)?;
        // Always, not only when this machine uploaded: another machine may have
        // uploaded since the last pass, and the rollups are what the dashboard
        // reads.
        let rollup = rollups::refresh(&store, &keys, &user_id, &now).map_err(cli_error)?;
        println!("{}", rollup.to_text());
        RunSummary {
            uploaded,
            unchanged,
        }
    };
    println!("{}", summary.to_text(args.dry_run));
    Ok(())
}

/// Without the bucket's salt this machine's hashes would not intersect anyone
/// else's, so a missing or malformed one stops the run rather than uploading
/// data nothing can merge.
fn configured_salt(config: &ConfigContext) -> Result<Salt> {
    let configured = config
        .sync()
        .and_then(|sync| sync.salt.clone())
        .ok_or_else(|| {
            cli_error(
                "this machine has no sync salt. Re-run 'ccusage sync setup' to adopt the bucket's."
                    .to_string(),
            )
        })?;
    Salt::parse(&configured).map_err(|error| cli_error(error.to_string()))
}

fn iso_now() -> String {
    format_rfc3339_millis(ccusage_core::TimestampMs::from_millis(now_ms()))
}

#[cfg(test)]
mod tests {
    use ccusage_sync::{FoldContext, FoldEntry, Salt};
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;

    const USER: &str = "user-1";
    const MACHINE: &str = "machine-1";
    const NOW: &str = "2026-09-17T18:12:03Z";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    fn salt() -> Salt {
        Salt::parse("00112233445566778899aabbccddeeff").expect("salt")
    }

    fn shards(salt: &Salt, tokens: u64) -> Vec<Shard> {
        let context = FoldContext {
            agent: AGENT,
            machine_id: MACHINE,
            user_id: USER,
            ccusage_version: "20.0.21",
            cost_mode: "auto",
            pricing_snapshot: "litellm@2026-09-16",
            generated_at: NOW,
            salt,
            redact_projects: true,
        };
        fold(
            &[FoldEntry {
                timestamp_ms: 1_789_603_650_000,
                model: "claude-sonnet-4-5".to_string(),
                input_tokens: tokens,
                session_id: "session-a".to_string(),
                project_path: "/home/me/dev/app".to_string(),
                message_id: Some("m1".to_string()),
                ..FoldEntry::default()
            }],
            &context,
        )
    }

    #[test]
    fn a_first_run_uploads_every_day_and_records_it() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let index = MachineIndex::default();

        let (uploads, unchanged) = plan_uploads(shards(&salt, 10), &index, AGENT);
        let written = upload(&store, &keys, USER, MACHINE, &uploads, NOW).expect("upload");

        assert_eq!(unchanged, 0);
        assert_eq!(written, vec!["2026-09-17".to_string()]);
        let key = keys
            .shard(
                USER,
                MACHINE,
                AGENT,
                UtcDate::new(2026, 9, 17).expect("date"),
            )
            .expect("key");
        assert!(store.get(&key).expect("get").is_some());
        let recorded = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        assert!(
            recorded
                .shards
                .contains_key(&MachineIndex::entry_key(AGENT, "2026-09-17"))
        );
    }

    /// Re-running over unchanged logs must cost nothing and rewrite nothing.
    #[test]
    fn a_second_run_over_the_same_logs_uploads_nothing() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let (uploads, _) = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(&store, &keys, USER, MACHINE, &uploads, NOW).expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let (uploads, unchanged) = plan_uploads(shards(&salt, 10), &index, AGENT);

        assert!(uploads.is_empty());
        assert_eq!(unchanged, 1);
    }

    #[test]
    fn a_day_that_gained_usage_is_uploaded_again() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let (uploads, _) = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(&store, &keys, USER, MACHINE, &uploads, NOW).expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let (uploads, unchanged) = plan_uploads(shards(&salt, 20), &index, AGENT);

        assert_eq!(uploads.len(), 1);
        assert_eq!(unchanged, 0);
    }

    /// The index must never claim a shard the bucket does not hold.
    #[test]
    fn nothing_is_recorded_when_there_is_nothing_to_upload() {
        let store = MemoryStore::new();
        let keys = keys();

        let written = upload(&store, &keys, USER, MACHINE, &[], NOW).expect("upload");

        assert!(written.is_empty());
        assert!(
            store
                .get(&keys.machine_index(USER, MACHINE).expect("key"))
                .expect("get")
                .is_none()
        );
    }

    #[test]
    fn a_summary_says_what_a_dry_run_would_have_done() {
        let summary = RunSummary {
            uploaded: vec!["2026-09-17".to_string()],
            unchanged: 2,
        };

        assert!(summary.to_text(true).starts_with("Would upload 1 day(s)"));
        assert!(summary.to_text(false).starts_with("Uploaded 1 day(s)"));
        assert_eq!(
            RunSummary::default().to_text(false),
            "Nothing to sync; 0 days already current."
        );
    }

    #[test]
    fn a_malformed_date_is_refused_rather_than_written_to_a_guessed_key() {
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("2026-09-17").is_ok());
    }
}
