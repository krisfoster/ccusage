//! The merge matrices from `specs/sync-merge-test-plan.md`, driven through the
//! real write path (`run::commit`) against a store that can be made to fail at
//! a chosen request.
//!
//! Every case ends the same way: whatever the bucket holds after the failure or
//! the race, a clean rerun has to agree with a rollup recomputed from nothing
//! but the shards. That comparison is the point — a bucket can be internally
//! consistent and still be wrong, and only the recomputation catches it.

use std::sync::Arc;

use ccusage_objectstore::{KeySpace, ObjectStore, Precondition, RollupKind};
use ccusage_sync::rollup::Daily;
use ccusage_sync::shard::{Cell, Dedupe, DedupeKey, SHARD_SCHEMA, Shard};
use ccusage_test_support::objectstore::{Fault, MemoryStore, Op, When};

use super::machine;
use super::maintenance;
use super::rollups;
use super::run::{self, UploadPlan};

pub(super) const USER: &str = "user-1";
const AGENT: &str = "claude";
pub(super) const NOW: &str = "2026-09-17T18:12:03Z";
/// Well before any of the test dates settle, so nothing is finalized unless a
/// case asks for it.
pub(super) const NOW_MS: i64 = 1_789_664_000_000;

/// A bucket plus the machine-side moves a run makes against it.
///
/// The store is shared so a fault hook can hold a second handle and run a
/// whole competing sync from inside another one's request.
#[derive(Clone)]
pub(super) struct Bucket {
    pub(super) store: Arc<MemoryStore>,
    pub(super) keys: KeySpace,
}

impl Bucket {
    pub(super) fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            keys: KeySpace::new("ccusage/v1").expect("prefix"),
        }
    }

    /// One `ccusage sync run`, minus reading local logs: the shards are given.
    pub(super) fn sync(&self, machine_id: &str, shards: Vec<Shard>) -> Result<Vec<String>, String> {
        self.sync_at(machine_id, shards, NOW_MS)
    }

    fn sync_at(
        &self,
        machine_id: &str,
        shards: Vec<Shard>,
        now_ms: i64,
    ) -> Result<Vec<String>, String> {
        let plan = self.plan(machine_id, shards)?;
        run::commit(
            self.store.as_ref(),
            &self.keys,
            USER,
            machine_id,
            &plan,
            None,
            NOW,
            now_ms,
        )
        .map(|commit| commit.written)
    }

    fn plan(&self, machine_id: &str, shards: Vec<Shard>) -> Result<UploadPlan, String> {
        let index = machine::load_index(self.store.as_ref(), &self.keys, USER, machine_id)?;
        Ok(run::plan_uploads(shards, &index, AGENT))
    }

    fn daily(&self) -> Daily {
        let (body, _) = self
            .store
            .get(&self.keys.rollup(RollupKind::Daily))
            .expect("get daily")
            .expect("daily exists");
        serde_json::from_slice(&body).expect("parse daily")
    }

    pub(super) fn input_tokens(&self) -> u64 {
        self.daily().totals().input_tokens
    }

    /// The totals a bucket with the same shards but no derived state would
    /// have: rollups thrown away and rebuilt in one pass.
    pub(super) fn recomputed_input_tokens(&self) -> u64 {
        let fresh = self.store.fork(&["rollup/"]);
        rollups::refresh(&fresh, &self.keys, USER, NOW).expect("recompute");
        let (body, _) = fresh
            .get(&self.keys.rollup(RollupKind::Daily))
            .expect("get daily")
            .expect("daily exists");
        serde_json::from_slice::<Daily>(&body)
            .expect("parse daily")
            .totals()
            .input_tokens
    }

    pub(super) fn shard_keys(&self) -> Vec<String> {
        self.store
            .keys()
            .into_iter()
            .filter(|key| key.contains("/shards/"))
            .collect()
    }

    pub(super) fn index_dates(&self, machine_id: &str) -> Vec<String> {
        machine::load_index(self.store.as_ref(), &self.keys, USER, machine_id)
            .expect("index")
            .shards
            .keys()
            .cloned()
            .collect()
    }

    pub(super) fn refresh(&self) -> Result<rollups::RollupSummary, String> {
        rollups::refresh(self.store.as_ref(), &self.keys, USER, NOW)
    }

    fn prune(&self, keep_days: u32, now_ms: i64) -> Result<maintenance::PruneSummary, String> {
        maintenance::prune(
            self.store.as_ref(),
            &self.keys,
            USER,
            keep_days,
            now_ms,
            false,
        )
    }

    fn forget(&self, machine_id: &str) -> Result<maintenance::ForgetSummary, String> {
        maintenance::forget(self.store.as_ref(), &self.keys, USER, machine_id, NOW)
    }

    pub(super) fn repair(&self) -> Result<maintenance::RepairSummary, String> {
        maintenance::repair(self.store.as_ref(), &self.keys, USER, NOW, NOW_MS, false)
    }

    fn merge_machine(&self, from: &str, into: &str) -> Result<maintenance::MergeSummary, String> {
        maintenance::merge_machine(self.store.as_ref(), &self.keys, USER, from, into, NOW)
    }
}

/// The invariant behind every row of the matrix: the incremental bucket agrees
/// with one recomputed from the shards alone.
pub(super) fn assert_converged(bucket: &Bucket) {
    assert_eq!(
        bucket.input_tokens(),
        bucket.recomputed_input_tokens(),
        "incremental totals disagree with a clean recomputation"
    );
}

fn shard(machine_id: &str, date: &str, input: u64) -> Shard {
    shard_with_keys(machine_id, date, input, &[input])
}

pub(super) fn shard_with_keys(machine_id: &str, date: &str, input: u64, dedupe: &[u64]) -> Shard {
    let mut shard = Shard {
        schema: SHARD_SCHEMA,
        agent: AGENT.to_string(),
        machine_id: machine_id.to_string(),
        user_id: USER.to_string(),
        utc_date: date.to_string(),
        generated_at: NOW.to_string(),
        ccusage_version: "20.0.21".to_string(),
        cost_mode: "auto".to_string(),
        pricing_snapshot: "litellm@2026-09-16".to_string(),
        cells: vec![Cell {
            bucket: 4,
            model: "claude-sonnet-4-5".to_string(),
            input_tokens: input,
            output_tokens: 10,
            cost: 1.0,
            messages: 1,
            keys: dedupe.iter().copied().map(DedupeKey).collect(),
            ..Cell::default()
        }],
        sessions: Vec::new(),
        dedupe: Dedupe {
            algo: "sha256-64/v1".to_string(),
            salt: "salt:abcd".to_string(),
            count: dedupe.len() as u64,
        },
        content_hash: None,
    };
    shard.finish().expect("finish");
    shard.content_hash = None;
    shard
}

fn days(count: u64) -> Vec<Shard> {
    (0..count)
        .map(|n| shard("aaaa", &format!("2026-09-{:02}", 10 + n), 100 * (n + 1)))
        .collect()
}

// --- S: the states a bucket can be in when a run starts -------------------

/// S1: nothing in the bucket, several days of local usage.
#[test]
fn an_empty_bucket_takes_every_local_day() {
    let bucket = Bucket::new();

    let written = bucket.sync("aaaa", days(3)).expect("sync");

    assert_eq!(written.len(), 3);
    assert_eq!(bucket.shard_keys().len(), 3);
    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// S2: nothing anywhere. The rollups still have to exist, or the dashboard has
/// nothing to fetch and cannot tell "no usage" from "never synced".
#[test]
fn an_empty_bucket_with_no_usage_still_writes_rollups() {
    let bucket = Bucket::new();

    let written = bucket.sync("aaaa", Vec::new()).expect("sync");

    assert!(written.is_empty());
    assert_eq!(bucket.input_tokens(), 0);
    assert!(bucket.store.keys().iter().any(|key| key.contains("daily")));
}

/// S3: the same logs twice. The second run must cost nothing and change
/// nothing — this is the steady state, run by a cron job all day.
#[test]
fn an_unchanged_rerun_uploads_nothing_and_rereads_no_shards() {
    let bucket = Bucket::new();
    bucket.sync("aaaa", days(3)).expect("first");
    bucket.store.reset_counts();

    let plan = bucket.plan("aaaa", days(3)).expect("plan");
    let written = bucket.sync("aaaa", days(3)).expect("second");

    assert_eq!(plan.unchanged, 3);
    assert!(written.is_empty());
    assert_eq!(bucket.store.op_count(Op::Put, "/shards/"), 0);
    assert_eq!(bucket.store.op_count(Op::Get, "/shards/"), 0);
    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// S4: a day grows. The day is replaced, not added to twice.
#[test]
fn a_changed_day_replaces_its_old_total_rather_than_adding_to_it() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 175)])
        .expect("second");

    assert_eq!(bucket.input_tokens(), 175);
    assert_eq!(bucket.shard_keys().len(), 1);
    assert_converged(&bucket);
}

/// S5: a day disappears locally — logs rotated away. The bucket keeps it: the
/// machine's absence of a log is not evidence the usage never happened, and
/// deleting history on a log rotation would be the worst kind of quiet loss.
#[test]
fn a_day_that_vanishes_locally_stays_in_the_bucket() {
    let bucket = Bucket::new();
    bucket.sync("aaaa", days(3)).expect("first");

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("second");

    assert_eq!(bucket.shard_keys().len(), 3);
    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// S6: two machines, different work.
#[test]
fn two_machines_with_disjoint_usage_are_summed() {
    let bucket = Bucket::new();

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("a");
    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("b");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// S7: the same logs on two machines — a synced home directory. Counted once.
#[test]
fn two_machines_reporting_the_same_entries_count_once() {
    let bucket = Bucket::new();

    bucket
        .sync(
            "aaaa",
            vec![shard_with_keys("aaaa", "2026-09-10", 100, &[1, 2])],
        )
        .expect("a");
    bucket
        .sync(
            "bbbb",
            vec![shard_with_keys("bbbb", "2026-09-10", 100, &[1, 2])],
        )
        .expect("b");

    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// S8: overlapping but not identical. Suppressing the cell would delete the
/// entries only one machine saw, so both are counted and the overlap is
/// flagged instead.
#[test]
fn a_partial_overlap_is_flagged_rather_than_suppressed() {
    let bucket = Bucket::new();

    bucket
        .sync(
            "aaaa",
            vec![shard_with_keys("aaaa", "2026-09-10", 100, &[1, 2])],
        )
        .expect("a");
    bucket
        .sync(
            "bbbb",
            vec![shard_with_keys("bbbb", "2026-09-10", 100, &[2, 3])],
        )
        .expect("b");

    assert_eq!(bucket.input_tokens(), 200);
    let daily = bucket.daily();
    let cells = &daily.days["2026-09-10"];
    assert!(cells.iter().all(|cell| cell.suppressed.is_none()));
    assert!(cells.iter().any(|cell| !cell.notes.is_empty()));
    assert_converged(&bucket);
}

/// S10: the machine is no longer on the manifest roster — a `sync forget` run
/// elsewhere, or a restored manifest. Its shards would be invisible, so the
/// run puts it back.
#[test]
fn a_machine_missing_from_the_roster_re_registers_itself() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");
    bucket
        .store
        .put(
            &bucket.keys.manifest(),
            br#"{"schema":1,"userId":"user-1","machines":[]}"#,
            "application/json",
            &Precondition::None,
        )
        .expect("clear roster");

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
        .expect("second");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

// --- F: a run that stops part-way ----------------------------------------

/// F3: the network drops during the third of five shard uploads. Two shards
/// are in the bucket with nothing pointing at them, so they count for nothing
/// until the rerun re-uploads and indexes them.
#[test]
fn a_failure_mid_upload_leaves_orphans_that_the_next_run_adopts() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("/shards/").skip(2), Fault::Network);

    let error = bucket.sync("aaaa", days(5)).expect_err("interrupted");

    assert!(
        error.contains("could not be reached") && error.contains("re-run"),
        "unhelpful error: {error}"
    );
    assert_eq!(bucket.shard_keys().len(), 2);
    assert!(bucket.index_dates("aaaa").is_empty());
    rollups::refresh(bucket.store.as_ref(), &bucket.keys, USER, NOW).expect("rollups");
    assert_eq!(bucket.input_tokens(), 0, "orphan shards must not count");

    let written = bucket.sync("aaaa", days(5)).expect("retry");

    assert_eq!(written.len(), 5);
    assert_eq!(bucket.input_tokens(), 1500);
    assert_converged(&bucket);
}

/// F4: every shard lands, then the index write fails. Same shape as F3 and the
/// same repair, which is why the index is written last.
#[test]
fn a_failure_before_the_index_write_is_repaired_by_the_next_run() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("index.json"), Fault::Network);

    bucket.sync("aaaa", days(3)).expect_err("interrupted");

    assert_eq!(bucket.shard_keys().len(), 3);
    assert!(bucket.index_dates("aaaa").is_empty());

    bucket.sync("aaaa", days(3)).expect("retry");

    assert_eq!(bucket.input_tokens(), 600);
    assert_eq!(bucket.shard_keys().len(), 3);
    assert_converged(&bucket);
}

/// F5: the shard is written and the caller is told it was not. The retry
/// overwrites the same key, so the lost response costs a re-upload and
/// nothing else — no second copy of the day, no doubled total.
#[test]
fn a_lost_shard_response_does_not_become_a_duplicate_day() {
    let bucket = Bucket::new();
    bucket
        .store
        .lose_response_on(When::put("/shards/"), Fault::Network);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("lost response");
    assert_eq!(bucket.shard_keys().len(), 1);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("retry");

    assert_eq!(bucket.shard_keys().len(), 1);
    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// F8: the index write succeeds and the response is lost. The retry reads the
/// index it does not know it wrote, finds the day current, and uploads
/// nothing.
#[test]
fn a_lost_index_response_leaves_the_next_run_with_nothing_to_do() {
    let bucket = Bucket::new();
    bucket
        .store
        .lose_response_on(When::put("index.json"), Fault::Network);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("lost response");
    assert_eq!(bucket.index_dates("aaaa"), vec!["claude/2026-09-10"]);

    let plan = bucket
        .plan("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("plan");
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("retry");

    assert_eq!(plan.unchanged, 1);
    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// F6: the index write fails on a conflict-free error after a previous run
/// already recorded other days. The days already indexed stay indexed.
#[test]
fn an_index_write_failure_does_not_lose_the_days_already_recorded() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");
    bucket
        .store
        .fail_on(When::put("index.json"), Fault::Server { status: 503 });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
        .expect_err("interrupted");

    assert_eq!(bucket.index_dates("aaaa"), vec!["claude/2026-09-10"]);
    assert_eq!(bucket.input_tokens(), 100);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
        .expect("retry");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// F12: the daily rollup cannot be written. The shards and the index are
/// already durable, so the day is not lost — it is only invisible until a pass
/// completes.
#[test]
fn a_daily_rollup_failure_leaves_the_shards_durable() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("daily.json"), Fault::Network);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("interrupted");

    assert_eq!(bucket.shard_keys().len(), 1);
    assert_eq!(bucket.index_dates("aaaa"), vec!["claude/2026-09-10"]);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("retry");

    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// F14: the daily rollup is written and `keys.json` is not. The dedupe keys
/// behind it are then missing, so the danger is the next pass double-counting
/// a second machine's copy of the same entries.
#[test]
fn a_missing_key_index_does_not_double_count_the_other_machine() {
    let bucket = Bucket::new();
    bucket.store.fail_on(When::put("keys.json"), Fault::Network);
    bucket
        .sync(
            "aaaa",
            vec![shard_with_keys("aaaa", "2026-09-10", 100, &[1, 2])],
        )
        .expect_err("interrupted");

    bucket
        .sync(
            "bbbb",
            vec![shard_with_keys("bbbb", "2026-09-10", 100, &[1, 2])],
        )
        .expect("second machine");

    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// F15/F16: the pass dies between the derived views, leaving `weekly.json`
/// older than `daily.json`. The next pass rewrites them from the daily rollup,
/// so they agree again without anyone re-reading a shard.
#[test]
fn derived_views_left_behind_catch_up_on_the_next_pass() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("weekly.json"), Fault::Network);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("interrupted");
    assert!(!bucket.store.keys().iter().any(|key| key.contains("weekly")));

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
        .expect("retry");

    let (body, _) = bucket
        .store
        .get(&bucket.keys.rollup(RollupKind::Weekly))
        .expect("get")
        .expect("weekly exists");
    let weekly: serde_json::Value = serde_json::from_slice(&body).expect("parse");
    let weekly_input: u64 = weekly["periods"]
        .as_array()
        .expect("periods")
        .iter()
        .map(|period| period["totals"]["inputTokens"].as_u64().unwrap_or_default())
        .sum();
    assert_eq!(weekly_input, bucket.input_tokens());
    assert_converged(&bucket);
}

/// F17: the credential expires mid-run. The error has to name re-authentication
/// rather than read as a network blip, and the bucket still converges.
#[test]
fn an_expired_credential_is_reported_as_one_and_the_bucket_converges() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("/shards/").skip(1), Fault::Unauthenticated);

    let error = bucket.sync("aaaa", days(3)).expect_err("expired");

    assert!(
        error.contains("sync setup") || error.contains("credential"),
        "does not tell the user to re-authenticate: {error}"
    );

    bucket.sync("aaaa", days(3)).expect("retry");
    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// F1: the very first request of the run fails, so the bucket is never
/// touched. The interesting part is not the error but what the next run
/// believes: it has to behave as a first run rather than as a resumed one.
#[test]
fn a_failure_before_the_first_write_leaves_a_run_with_nothing_to_resume() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::get("manifest.json"), Fault::Network);

    bucket.sync("aaaa", days(3)).expect_err("never started");

    assert!(bucket.store.keys().is_empty(), "the bucket was written to");

    let written = bucket.sync("aaaa", days(3)).expect("first real run");

    assert_eq!(written.len(), 3);
    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// F7: the index compare-and-swap loses every one of its attempts. Giving up
/// is the correct outcome — the alternative is overwriting whatever the other
/// writer recorded — and the shards it already uploaded stay durable, so the
/// next run is cheap.
#[test]
fn an_index_starved_of_its_cas_attempts_gives_up_without_losing_the_shards() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("index.json").times(5), Fault::Conflict);

    let error = bucket.sync("aaaa", days(3)).expect_err("starved");

    assert!(
        error.contains("kept changing"),
        "does not name the contention: {error}"
    );
    assert_eq!(bucket.shard_keys().len(), 3);
    assert!(bucket.index_dates("aaaa").is_empty());

    bucket
        .sync("aaaa", days(3))
        .expect("retry once it is quiet");

    assert_eq!(bucket.input_tokens(), 600);
    assert_converged(&bucket);
}

/// F9: the run dies after the index and before `machine.json`. `lastSyncAt` is
/// advisory, so the merge must be indifferent to it — asserted by counting the
/// reads rather than by reasoning about the code.
#[test]
fn a_stale_machine_record_does_not_affect_the_merge() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("machine.json"), Fault::Network);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("died before the machine record");

    assert_eq!(bucket.index_dates("aaaa"), vec!["claude/2026-09-10"]);
    assert!(
        !bucket
            .store
            .keys()
            .iter()
            .any(|key| key.ends_with("machine.json")),
        "the machine record was written after all"
    );

    bucket.store.reset_counts();
    bucket.refresh().expect("rollups");

    assert_eq!(bucket.store.op_count(Op::Get, "machine.json"), 0);
    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// F10: a prune that dies half way through its deletes. The index entries are
/// withdrawn first, so what is left behind is two orphan objects — a state the
/// merge already ignores — rather than promises with nothing behind them. The
/// totals drop the pruned days immediately, and re-running the prune clears
/// the leftovers.
///
/// Written the other way round, the leftover would be silent: a rollup pass
/// skips a day whose hash it already has, so it would never discover that the
/// object had gone, and the day would keep counting.
#[test]
fn an_interrupted_prune_leaves_orphans_rather_than_dangling_promises() {
    let bucket = Bucket::new();
    let much_later_ms = NOW_MS + 400 * 24 * 60 * 60 * 1000;
    bucket.sync("aaaa", days(4)).expect("first");
    bucket
        .store
        .fail_on(When::delete("/shards/").skip(2), Fault::Network);

    bucket.prune(30, much_later_ms).expect_err("interrupted");

    assert_eq!(bucket.shard_keys().len(), 2, "two deletes should have run");
    assert!(
        bucket.index_dates("aaaa").is_empty(),
        "a promise outlived the object it names"
    );

    let summary = bucket.refresh().expect("rollups");

    assert_eq!(summary.missing, 0);
    assert_eq!(bucket.input_tokens(), 0, "pruned days left the totals");
    assert_converged(&bucket);

    bucket.prune(30, much_later_ms).expect("finish the prune");

    assert!(bucket.shard_keys().is_empty());
    assert_converged(&bucket);
}

/// F11: the rollup pass cannot read a shard it needs. Nothing is published
/// from a partial read — the daily rollup keeps the totals it had — and the
/// next pass reads the shard and catches up.
#[test]
fn a_shard_read_failure_publishes_nothing_and_retries_cleanly() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");
    bucket
        .store
        .fail_on(When::get("/shards/"), Fault::Server { status: 503 });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 175)])
        .expect_err("could not re-read the day");

    assert_eq!(bucket.input_tokens(), 100, "a partial read was published");

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 175)])
        .expect("retry");

    assert_eq!(bucket.input_tokens(), 175);
    assert_converged(&bucket);
}

/// F13: the daily rollup's compare-and-swap loses all five attempts, then the
/// contention stops. C12 pins the error; this pins the recovery — the winner's
/// totals stand in the meantime, and the next pass folds this run's day in.
#[test]
fn a_daily_rollup_starved_of_its_cas_attempts_catches_up_afterwards() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");
    bucket
        .store
        .fail_on(When::put("daily.json").times(5), Fault::Conflict);

    let error = bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
        .expect_err("starved");

    assert!(
        error.contains("kept changing"),
        "does not name the contention: {error}"
    );
    assert_eq!(bucket.input_tokens(), 100, "the last good totals stand");
    assert_eq!(
        bucket.index_dates("aaaa").len(),
        2,
        "the day is vouched for"
    );

    bucket.refresh().expect("retry once it is quiet");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// F18: `forget` unregisters the machine and then fails before deleting its
/// shards. The roster is what the rollups read, so the usage goes quiet while
/// the objects are still there — recoverable in either direction, by finishing
/// the forget or by repairing the machine back onto the roster.
#[test]
fn a_forget_that_dies_after_unregistering_can_be_finished_or_undone() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("a");
    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("b");
    bucket
        .store
        .fail_on(When::delete("/shards/"), Fault::Network);

    bucket.forget("aaaa").expect_err("interrupted");

    let roster =
        super::bootstrap::manifest_machines(bucket.store.as_ref(), &bucket.keys).expect("roster");
    assert_eq!(roster, vec!["bbbb".to_string()]);
    assert!(
        bucket.shard_keys().iter().any(|key| key.contains("aaaa")),
        "the shards were deleted after all"
    );

    bucket.refresh().expect("rollups");
    assert_eq!(
        bucket.input_tokens(),
        50,
        "an unregistered machine is quiet"
    );
    assert_converged(&bucket);

    bucket.repair().expect("repair");

    assert_eq!(bucket.input_tokens(), 150, "repair puts the machine back");
    assert_converged(&bucket);

    bucket.forget("aaaa").expect("finish the forget");

    assert_eq!(bucket.input_tokens(), 50);
    assert_converged(&bucket);
}

// --- C: two runs at once --------------------------------------------------

/// C1: the same machine, the same logs, twice over — the second run slipping
/// in between the first's shard write and its index write. The day must be
/// counted once, and the shard written once. Only the remote CAS is in play:
/// the local lock is what makes this rare, not what makes it safe.
#[test]
fn a_second_run_between_shard_and_index_does_not_double_count() {
    let bucket = Bucket::new();
    let interleaved = bucket.clone();
    bucket.store.before(When::put("index.json"), move || {
        interleaved
            .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
            .expect("interleaved run");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("outer run");

    assert_eq!(bucket.shard_keys().len(), 1);
    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// C2: two runs of one machine covering different days, overlapping at the
/// index. The loser of the CAS re-reads and re-applies, so neither day is
/// dropped from the index.
#[test]
fn two_runs_of_one_machine_on_different_days_keep_both() {
    let bucket = Bucket::new();
    let interleaved = bucket.clone();
    bucket.store.before(When::put("index.json"), move || {
        interleaved
            .sync("aaaa", vec![shard("aaaa", "2026-09-11", 50)])
            .expect("interleaved run");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("outer run");

    assert_eq!(
        bucket.index_dates("aaaa"),
        vec!["claude/2026-09-10", "claude/2026-09-11"]
    );
    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C3: two runs of one machine disagreeing about the same day — a log the
/// second read a moment later. One of the two contents wins the shard, and
/// the totals have to match whichever it was rather than counting both.
#[test]
fn two_runs_of_one_machine_on_one_day_agree_with_the_shard_that_won() {
    let bucket = Bucket::new();
    let interleaved = bucket.clone();
    bucket.store.before(When::put("index.json"), move || {
        interleaved
            .sync("aaaa", vec![shard("aaaa", "2026-09-10", 175)])
            .expect("interleaved run");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("outer run");

    assert_eq!(bucket.shard_keys().len(), 1);
    assert_converged(&bucket);
}

/// C4/C5: a rollup pass while another machine is uploading. The daily rollup
/// is compare-and-swapped, so the pass that loses re-reads and re-applies
/// instead of publishing totals that forgot the other machine's day.
#[test]
fn a_rollup_pass_racing_an_upload_keeps_both_machines() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first machine");
    let interleaved = bucket.clone();
    bucket.store.before(When::put("daily.json"), move || {
        interleaved
            .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
            .expect("interleaved run");
    });

    rollups::refresh(bucket.store.as_ref(), &bucket.keys, USER, NOW).expect("refresh");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C6/C7: two machines syncing at once, neither yet on the roster. A roster
/// write that clobbered the other would make a machine's shards invisible.
#[test]
fn two_machines_registering_at_once_both_stay_on_the_roster() {
    let bucket = Bucket::new();
    let interleaved = bucket.clone();
    bucket.store.before(When::put("manifest.json"), move || {
        interleaved
            .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
            .expect("interleaved run");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("outer run");

    let machines =
        super::bootstrap::manifest_machines(bucket.store.as_ref(), &bucket.keys).expect("roster");
    assert_eq!(machines, vec!["aaaa".to_string(), "bbbb".to_string()]);
    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C8: one machine dies after writing a shard, and another machine syncs
/// before it comes back. The orphan must not be counted — nothing vouches for
/// it — and must not disturb the machine that is running.
#[test]
fn an_orphan_shard_from_a_dead_run_does_not_disturb_another_machine() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("index.json"), Fault::Network);
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("died after the shard write");

    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("other machine");

    assert_eq!(bucket.input_tokens(), 50);
    assert_converged(&bucket);

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first machine returns");
    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C9: the run dies after the index write but before the rollups, and the next
/// pass is another machine's. The day is already vouched for, so that pass
/// picks it up without the first machine running again.
#[test]
fn a_day_indexed_by_a_dead_run_is_rolled_up_by_the_next_machine() {
    let bucket = Bucket::new();
    bucket
        .store
        .fail_on(When::put("daily.json"), Fault::Network);
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect_err("died before the rollups");

    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("other machine");

    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C12: a bucket under constant write pressure. The CAS loop is bounded, so
/// it has to give up with an error that names the contention rather than
/// spinning forever or writing over the other machine.
#[test]
fn unending_contention_fails_loudly_rather_than_overwriting() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("first");
    bucket
        .store
        .fail_on(When::put("daily.json").always(), Fault::Conflict);

    let error =
        rollups::refresh(bucket.store.as_ref(), &bucket.keys, USER, NOW).expect_err("starved");

    assert!(
        error.contains("daily") || error.contains("sync"),
        "unhelpful contention error: {error}"
    );
    assert_eq!(bucket.input_tokens(), 100, "the last good totals stand");
}

/// C10: one machine syncing while another is being forgotten. The forget must
/// take only its own machine's usage with it: the live machine's day was
/// already durable when the forget started, and nothing in the forget's
/// rollup rebuild may drop it.
#[test]
fn a_forget_running_under_a_sync_takes_only_its_own_machine() {
    let bucket = Bucket::new();
    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("the machine being forgotten");
    let interleaved = bucket.clone();
    bucket.store.before(When::put("daily.json"), move || {
        interleaved.forget("bbbb").expect("interleaved forget");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("live machine");

    let roster =
        super::bootstrap::manifest_machines(bucket.store.as_ref(), &bucket.keys).expect("roster");
    assert_eq!(roster, vec!["aaaa".to_string()]);
    assert!(
        bucket.shard_keys().iter().all(|key| !key.contains("bbbb")),
        "the forgotten machine's shards are still there"
    );
    assert_eq!(
        bucket.input_tokens(),
        100,
        "the live machine's day survived"
    );
    assert_converged(&bucket);
}

/// C11: a sync racing a prune whose retention window has moved past the day
/// being uploaded. The prune deletes that shard while it is still an orphan,
/// and the sync then indexes it, so the run ends with a promise the bucket
/// cannot keep. That is the worst case for this pair, and what it must not be
/// is silent: the next rollup pass reports the gap, and `repair` clears it.
#[test]
fn a_prune_running_under_a_sync_leaves_only_reported_gaps() {
    let bucket = Bucket::new();
    let much_later_ms = NOW_MS + 400 * 24 * 60 * 60 * 1000;
    bucket.sync("aaaa", days(3)).expect("history");
    let interleaved = bucket.clone();
    bucket.store.before(When::put("index.json"), move || {
        interleaved
            .prune(30, much_later_ms)
            .expect("interleaved prune");
    });

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-20", 25)])
        .expect("the day being synced");

    assert!(bucket.shard_keys().is_empty(), "the prune took every day");
    let summary = bucket.refresh().expect("rollups");
    assert_eq!(summary.missing, 1, "a gap nobody reported");
    assert_eq!(bucket.input_tokens(), 0, "nothing is counted twice or lost");
    assert_converged(&bucket);

    bucket.repair().expect("repair");

    assert!(bucket.index_dates("aaaa").is_empty());
    assert_eq!(bucket.refresh().expect("rollups").missing, 0);
    assert_converged(&bucket);
}

/// C13: two merges of the same machine at once. One of them has to lose —
/// the shards it is moving are gone by the time it looks — and losing has to
/// mean an error, not a half-moved machine or a doubled day.
#[test]
fn two_merges_of_one_machine_do_not_duplicate_or_lose_it() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("the machine being merged away");
    bucket
        .sync("bbbb", vec![shard("bbbb", "2026-09-11", 50)])
        .expect("the surviving machine");
    let interleaved = bucket.clone();
    bucket.store.before(When::put("/shards/"), move || {
        interleaved
            .merge_machine("aaaa", "bbbb")
            .expect("interleaved merge");
    });

    let second = bucket.merge_machine("aaaa", "bbbb");

    assert!(
        second.is_err(),
        "both merges claimed to move the same shards: {second:?}"
    );
    let roster =
        super::bootstrap::manifest_machines(bucket.store.as_ref(), &bucket.keys).expect("roster");
    assert_eq!(roster, vec!["bbbb".to_string()]);
    assert_eq!(bucket.shard_keys().len(), 2, "a day was copied twice");
    bucket.repair().expect("repair");
    assert_eq!(bucket.input_tokens(), 150, "the merged usage is intact");
    assert_converged(&bucket);
}

// --- C, on real threads ---------------------------------------------------
//
// The barrier-driven rows above script one interleaving each, which assumes
// away the schedules a thread pair would actually find. These run the same
// three shapes on two OS threads against the shared store, so the interleaving
// is the scheduler's choice: the assertion is not that a particular thread
// wins but that whatever happens, the bucket converges and no run ends with an
// error other than the bounded-contention one it is allowed to report.

/// Two syncs at once, each allowed to lose the CAS race and say so.
fn race(bucket: &Bucket, left: (&str, Vec<Shard>), right: (&str, Vec<Shard>)) {
    let (left_id, left_shards) = (left.0.to_string(), left.1);
    let (right_id, right_shards) = (right.0.to_string(), right.1);
    let one = bucket.clone();
    let two = bucket.clone();

    let (first, second) = std::thread::scope(|scope| {
        let left = scope.spawn(move || one.sync(&left_id, left_shards));
        let right = scope.spawn(move || two.sync(&right_id, right_shards));
        (
            left.join().expect("left thread"),
            right.join().expect("right thread"),
        )
    });

    for outcome in [first, second] {
        if let Err(error) = outcome {
            assert!(
                error.contains("kept changing"),
                "a racing run failed for a reason other than contention: {error}"
            );
        }
    }
}

/// C1 on real threads: one machine, one day, run twice at once. Whoever wins,
/// the day is stored once and counted once.
#[test]
fn two_real_threads_syncing_the_same_day_count_it_once() {
    let bucket = Bucket::new();

    race(
        &bucket,
        ("aaaa", vec![shard("aaaa", "2026-09-10", 100)]),
        ("aaaa", vec![shard("aaaa", "2026-09-10", 100)]),
    );

    bucket
        .sync("aaaa", vec![shard("aaaa", "2026-09-10", 100)])
        .expect("settling run");
    assert_eq!(bucket.shard_keys().len(), 1);
    assert_eq!(bucket.input_tokens(), 100);
    assert_converged(&bucket);
}

/// C2 on real threads: one machine, a different day in each run — the logs
/// grew between the two folds. Neither day may be dropped from the index.
#[test]
fn two_real_threads_syncing_different_days_keep_both() {
    let bucket = Bucket::new();

    race(
        &bucket,
        ("aaaa", vec![shard("aaaa", "2026-09-10", 100)]),
        ("aaaa", vec![shard("aaaa", "2026-09-11", 50)]),
    );

    bucket
        .sync(
            "aaaa",
            vec![
                shard("aaaa", "2026-09-10", 100),
                shard("aaaa", "2026-09-11", 50),
            ],
        )
        .expect("settling run");
    assert_eq!(
        bucket.index_dates("aaaa"),
        vec!["claude/2026-09-10", "claude/2026-09-11"]
    );
    assert_eq!(bucket.input_tokens(), 150);
    assert_converged(&bucket);
}

/// C3 on real threads: one machine, one day, two different readings of it.
/// One body wins the key; the totals have to be that body's, not the sum.
#[test]
fn two_real_threads_disagreeing_about_a_day_agree_with_the_winner() {
    let bucket = Bucket::new();

    race(
        &bucket,
        ("aaaa", vec![shard("aaaa", "2026-09-10", 100)]),
        ("aaaa", vec![shard("aaaa", "2026-09-10", 175)]),
    );

    bucket.refresh().expect("settling refresh");
    assert_eq!(bucket.shard_keys().len(), 1);
    let total = bucket.input_tokens();
    assert!(
        total == 100 || total == 175,
        "the day was counted as neither reading: {total}"
    );
    assert_converged(&bucket);
}
