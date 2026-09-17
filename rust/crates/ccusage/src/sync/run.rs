//! `ccusage sync run`: fold this machine's local usage into shards and upload
//! the ones the bucket does not already have.
//!
//! The upload is incremental by content hash rather than by timestamp. A shard
//! whose hash matches the machine index is not re-uploaded, so a sync that
//! finds nothing new costs one read of the index and nothing else, and a day
//! that gains a single entry rewrites only that day.

use ccusage_config::ConfigContext;
use ccusage_objectstore::{KeySpace, ObjectStore, Precondition, UtcDate};
use ccusage_sync::{FoldContext, Salt, Shard, fold, is_finalized};

use super::machine::{self, IndexEntry, MachineIndex};
use super::now_ms;
use crate::{
    Result,
    cli::{SharedArgs, SyncRunArgs},
    cli_error, format_rfc3339_millis,
    pricing::PricingMap,
    sync::{failures, maintenance, rollups, sources},
};

/// What a run did, in the terms the user is told about.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunSummary {
    pub uploaded: Vec<String>,
    pub unchanged: usize,
    /// The agents that had usage on this machine, so the user can see at a
    /// glance that a tool they expected is missing.
    pub agents: Vec<String>,
    /// Agents whose logs could not be read, with the reason.
    pub skipped: Vec<String>,
    /// Days that changed after they had settled. Reported because a reader may
    /// already have cached a total this run has just moved.
    pub late_edits: Vec<String>,
}

impl RunSummary {
    pub fn to_text(&self, dry_run: bool) -> String {
        let mut text = if self.uploaded.is_empty() {
            format!("Nothing to sync; {} days already current.", self.unchanged)
        } else {
            let verb = if dry_run { "Would upload" } else { "Uploaded" };
            format!(
                "{verb} {} day(s): {}. {} already current.",
                self.uploaded.len(),
                self.uploaded.join(", "),
                self.unchanged
            )
        };
        if !self.agents.is_empty() {
            text.push_str(&format!(" Agents: {}.", self.agents.join(", ")));
        }
        if !self.skipped.is_empty() {
            text.push_str(&format!(
                " Warning: {} agent(s) could not be read and were skipped: {}.",
                self.skipped.len(),
                self.skipped.join("; ")
            ));
        }
        if !self.late_edits.is_empty() {
            text.push_str(&format!(
                " Warning: {} finalized day(s) changed and were rewritten: {}.",
                self.late_edits.len(),
                self.late_edits.join(", ")
            ));
        }
        text
    }
}

/// What an upload would do, decided before anything is written.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct UploadPlan {
    pub uploads: Vec<Shard>,
    pub unchanged: usize,
    /// Dates whose shard had been finalized and changed anyway.
    pub late_edits: Vec<String>,
}

impl UploadPlan {
    /// Folds another agent's plan into this one.
    ///
    /// A date is reported once however many agents touched it: the user is
    /// told which days moved, and "2026-09-17" three times says nothing more
    /// than once does.
    pub(crate) fn absorb(&mut self, other: Self) {
        self.uploads.extend(other.uploads);
        self.unchanged += other.unchanged;
        for date in other.late_edits {
            if !self.late_edits.contains(&date) {
                self.late_edits.push(date);
            }
        }
    }

    /// The dates this plan would upload, each named once and in order.
    pub(crate) fn dates(&self) -> Vec<String> {
        let mut dates: Vec<String> = self
            .uploads
            .iter()
            .map(|shard| shard.utc_date.clone())
            .collect();
        dates.sort_unstable();
        dates.dedup();
        dates
    }
}

/// Splits folded shards into the ones the bucket needs and the ones it has.
///
/// Both sides are driven by the shard's own content hash, which excludes the
/// generation timestamp, so re-running sync on unchanged logs uploads nothing.
/// A day that had settled and changed anyway is still uploaded — the machine's
/// own logs are the authority for its own usage, and refusing the write would
/// leave the bucket knowingly wrong — but it is reported rather than folded in
/// silently.
pub(crate) fn plan_uploads(shards: Vec<Shard>, index: &MachineIndex, agent: &str) -> UploadPlan {
    let mut plan = UploadPlan::default();
    for mut shard in shards {
        let Ok(hash) = shard.finish().map(str::to_string) else {
            continue;
        };
        if index.is_current(agent, &shard.utc_date, &hash) {
            plan.unchanged += 1;
            continue;
        }
        if index.is_finalized(agent, &shard.utc_date) {
            plan.late_edits.push(shard.utc_date.clone());
        }
        plan.uploads.push(shard);
    }
    plan
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
    now_ms: i64,
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
            .map_err(|error| failures::explain(&error))?;
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
            let entry_key = MachineIndex::entry_key(&shard.agent, &shard.utc_date);
            let previous = index.shards.get(&entry_key);
            let was_finalized = previous.is_some_and(|entry| entry.finalized);
            let late = was_finalized && previous.is_some_and(|entry| entry.content_hash != hash);
            index.shards.insert(
                entry_key,
                IndexEntry {
                    content_hash: hash,
                    // Once set, finalization stays set: a clock that jumps
                    // backwards must not un-settle a day and hide the edits
                    // that follow.
                    finalized: was_finalized || is_finalized(&shard.utc_date, now_ms),
                    updated_at: now.to_string(),
                    late_edits: previous.map_or(0, |entry| entry.late_edits) + u32::from(late),
                    last_late_edit_at: if late {
                        Some(now.to_string())
                    } else {
                        previous.and_then(|entry| entry.last_late_edit_at.clone())
                    },
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

pub(crate) fn execute(config: &ConfigContext, args: &SyncRunArgs) -> Result<()> {
    let salt = configured_salt(config)?;
    let super::Session {
        store,
        keys,
        user_id,
        machine_id,
    } = super::connect(config)?;

    // Before anything is folded: a clock hours out files usage under the
    // wrong dates, and no later step can tell that it did.
    failures::check_clock(bucket_time_ms(&store, &keys), now_ms()).map_err(cli_error)?;

    let shared = SharedArgs::with_defaults();
    let pricing =
        PricingMap::load_with_overrides(shared.offline, false, shared.pricing_overrides.iter());
    let loaded = sources::load_all(&shared, &pricing);
    let redact_projects = config
        .sync()
        .and_then(|sync| sync.redact_projects)
        .unwrap_or(true);
    let generated_at = iso_now();

    let index = machine::load_index(&store, &keys, &user_id, &machine_id).map_err(cli_error)?;
    let mut plan = UploadPlan::default();
    for agent in &loaded.agents {
        let context = FoldContext {
            agent: agent.agent,
            machine_id: &machine_id,
            user_id: &user_id,
            ccusage_version: env!("CARGO_PKG_VERSION"),
            cost_mode: "auto",
            pricing_snapshot: concat!("embedded@", env!("CARGO_PKG_VERSION")),
            generated_at: &generated_at,
            salt: &salt,
            redact_projects,
        };
        plan.absorb(plan_uploads(
            fold(&agent.entries, &context),
            &index,
            agent.agent,
        ));
    }
    let agents: Vec<String> = loaded.named().iter().map(|name| name.to_string()).collect();
    let skipped = skipped(&loaded);

    let summary = if args.dry_run {
        if let Some(keep_days) = args.prune {
            let pruned = maintenance::prune(&store, &keys, &user_id, keep_days, now_ms(), true)
                .map_err(cli_error)?;
            println!("{}", pruned.to_text());
        }
        RunSummary {
            uploaded: plan.dates(),
            unchanged: plan.unchanged,
            agents,
            skipped,
            late_edits: plan.late_edits,
        }
    } else {
        let now = iso_now();
        upload(
            &store,
            &keys,
            &user_id,
            &machine_id,
            &plan.uploads,
            &now,
            now_ms(),
        )
        .map_err(cli_error)?;
        machine::update_machine(&store, &keys, &user_id, &machine_id, |record| {
            record.last_sync_at = Some(now.clone());
        })
        .map_err(cli_error)?;
        // Before the rollups, so one pass both removes the old days and
        // rewrites the totals that mentioned them.
        if let Some(keep_days) = args.prune {
            let pruned = maintenance::prune(&store, &keys, &user_id, keep_days, now_ms(), false)
                .map_err(cli_error)?;
            println!("{}", pruned.to_text());
        }
        // Always, not only when this machine uploaded: another machine may have
        // uploaded since the last pass, and the rollups are what the dashboard
        // reads.
        let rollup = rollups::refresh(&store, &keys, &user_id, &now).map_err(cli_error)?;
        println!("{}", rollup.to_text());
        RunSummary {
            uploaded: plan.dates(),
            unchanged: plan.unchanged,
            agents,
            skipped,
            late_edits: plan.late_edits,
        }
    };
    println!("{}", summary.to_text(args.dry_run));
    Ok(())
}

/// Agents that failed to load, named with the reason so the user can tell a
/// tool they never installed from a log this build could not parse.
fn skipped(loaded: &sources::Sources) -> Vec<String> {
    loaded
        .failures
        .iter()
        .map(|failure| format!("{} ({})", failure.agent, failure.detail))
        .collect()
}

/// When the bucket's own clock last touched the manifest, as the reference
/// for this machine's clock. A bucket nobody has written yet has nothing to
/// compare against, and neither does a store that reports no times.
fn bucket_time_ms(store: &dyn ObjectStore, keys: &KeySpace) -> Option<i64> {
    let key = keys.manifest();
    store
        .get(&key)
        .ok()
        .flatten()
        .and_then(|(_, meta)| meta.updated_ms)
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
    use ccusage_test_support::objectstore::{Fault, MemoryStore};

    use super::*;

    /// The agent the fixtures fold under. Shards are keyed by agent, and the
    /// rules under test do not vary by which one.
    const AGENT: &str = "claude";
    const USER: &str = "user-1";
    const MACHINE: &str = "machine-1";
    const NOW: &str = "2026-09-17T18:12:03Z";
    const NOW_MS: i64 = 1_789_661_523_000;
    /// Far enough past 2026-09-18T00:00Z that 2026-09-17 has settled.
    const MUCH_LATER_MS: i64 = NOW_MS + 7 * 24 * 60 * 60 * 1000;

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

        let plan = plan_uploads(shards(&salt, 10), &index, AGENT);
        let written =
            upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect("upload");

        assert_eq!(plan.unchanged, 0);
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
        let plan = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let plan = plan_uploads(shards(&salt, 10), &index, AGENT);

        assert!(plan.uploads.is_empty());
        assert_eq!(plan.unchanged, 1);
    }

    /// Going offline part-way through must leave the index untouched, so the
    /// next run re-uploads rather than believing a shard it never wrote.
    #[test]
    fn an_upload_that_loses_the_network_records_nothing_and_says_a_re_run_resumes() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let plan = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        store.fail_next(Fault::Network);

        let error =
            upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect_err("offline");

        assert!(error.contains("could not be reached"), "{error}");
        assert!(
            machine::load_index(&store, &keys, USER, MACHINE)
                .expect("index")
                .shards
                .is_empty()
        );
        let retried = upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS)
            .expect("upload after reconnecting");
        assert_eq!(retried, vec!["2026-09-17".to_string()]);
    }

    /// The state a crash between the shard write and the index write leaves
    /// behind: the object is there, nothing points at it. The next run must
    /// converge on its own, without the user reaching for `sync repair`.
    #[test]
    fn a_shard_left_unindexed_by_a_crash_is_re_uploaded_and_indexed_next_run() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let mut orphan = shards(&salt, 10);
        let key = keys
            .shard(
                USER,
                MACHINE,
                AGENT,
                UtcDate::new(2026, 9, 17).expect("date"),
            )
            .expect("key");
        orphan[0].finish().expect("hash");
        store
            .put(
                &key,
                &orphan[0].to_json().expect("json"),
                "application/json",
                &Precondition::None,
            )
            .expect("orphan shard");

        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        let plan = plan_uploads(shards(&salt, 10), &index, AGENT);
        upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect("upload");

        assert_eq!(plan.uploads.len(), 1);
        let recorded = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        assert!(
            recorded
                .shards
                .contains_key(&MachineIndex::entry_key(AGENT, "2026-09-17"))
        );
    }

    #[test]
    fn a_day_that_gained_usage_is_uploaded_again() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let plan = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let plan = plan_uploads(shards(&salt, 20), &index, AGENT);

        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.unchanged, 0);
    }

    /// A day synced while it is still running must stay open, or every entry
    /// logged later that day would arrive as an anomaly.
    #[test]
    fn a_day_synced_while_it_is_running_is_not_finalized() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let plan = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);

        upload(&store, &keys, USER, MACHINE, &plan.uploads, NOW, NOW_MS).expect("upload");

        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        assert!(!index.is_finalized(AGENT, "2026-09-17"));
        assert!(plan.late_edits.is_empty());
    }

    #[test]
    fn a_day_synced_well_after_it_ended_is_finalized() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let plan = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);

        upload(
            &store,
            &keys,
            USER,
            MACHINE,
            &plan.uploads,
            NOW,
            MUCH_LATER_MS,
        )
        .expect("upload");

        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        assert!(index.is_finalized(AGENT, "2026-09-17"));
        // Settling a day for the first time is not itself a late edit.
        assert_eq!(
            index.shards[&MachineIndex::entry_key(AGENT, "2026-09-17")].late_edits,
            0
        );
    }

    /// The rewrite still happens — this machine's logs are the authority for
    /// its own usage — but it is counted and reported, not applied silently.
    #[test]
    fn rewriting_a_settled_day_is_recorded_and_still_uploaded() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let first = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(
            &store,
            &keys,
            USER,
            MACHINE,
            &first.uploads,
            NOW,
            MUCH_LATER_MS,
        )
        .expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let second = plan_uploads(shards(&salt, 20), &index, AGENT);
        upload(
            &store,
            &keys,
            USER,
            MACHINE,
            &second.uploads,
            NOW,
            MUCH_LATER_MS,
        )
        .expect("upload");

        assert_eq!(second.late_edits, vec!["2026-09-17".to_string()]);
        assert_eq!(second.uploads.len(), 1);
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        let entry = &index.shards[&MachineIndex::entry_key(AGENT, "2026-09-17")];
        assert_eq!(entry.late_edits, 1);
        assert_eq!(entry.last_late_edit_at.as_deref(), Some(NOW));
    }

    /// A clock that jumps backwards must not un-settle a day and hide the
    /// edits that follow it.
    #[test]
    fn a_settled_day_stays_settled_when_the_clock_goes_backwards() {
        let store = MemoryStore::new();
        let keys = keys();
        let salt = salt();
        let first = plan_uploads(shards(&salt, 10), &MachineIndex::default(), AGENT);
        upload(
            &store,
            &keys,
            USER,
            MACHINE,
            &first.uploads,
            NOW,
            MUCH_LATER_MS,
        )
        .expect("upload");
        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");

        let second = plan_uploads(shards(&salt, 20), &index, AGENT);
        upload(&store, &keys, USER, MACHINE, &second.uploads, NOW, NOW_MS).expect("upload");

        let index = machine::load_index(&store, &keys, USER, MACHINE).expect("index");
        assert!(index.is_finalized(AGENT, "2026-09-17"));
    }

    /// The index must never claim a shard the bucket does not hold.
    #[test]
    fn nothing_is_recorded_when_there_is_nothing_to_upload() {
        let store = MemoryStore::new();
        let keys = keys();

        let written = upload(&store, &keys, USER, MACHINE, &[], NOW, NOW_MS).expect("upload");

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
            agents: vec!["claude".to_string()],
            skipped: Vec::new(),
            late_edits: Vec::new(),
        };

        assert!(summary.to_text(true).starts_with("Would upload 1 day(s)"));
        assert!(summary.to_text(false).starts_with("Uploaded 1 day(s)"));
        assert_eq!(
            RunSummary::default().to_text(false),
            "Nothing to sync; 0 days already current."
        );
    }

    #[test]
    fn a_summary_warns_about_days_that_changed_after_they_settled() {
        let summary = RunSummary {
            uploaded: vec!["2026-09-10".to_string()],
            unchanged: 0,
            agents: vec!["claude".to_string()],
            skipped: Vec::new(),
            late_edits: vec!["2026-09-10".to_string()],
        };

        let text = summary.to_text(false);
        assert!(text.contains("Warning"), "{text}");
        assert!(text.contains("2026-09-10"), "{text}");
    }

    #[test]
    fn a_malformed_date_is_refused_rather_than_written_to_a_guessed_key() {
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("2026-09-17").is_ok());
    }
}
