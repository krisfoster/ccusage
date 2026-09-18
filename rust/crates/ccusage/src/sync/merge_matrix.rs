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
use super::rollups;
use super::run::{self, UploadPlan};

const USER: &str = "user-1";
const AGENT: &str = "claude";
const NOW: &str = "2026-09-17T18:12:03Z";
/// Well before any of the test dates settle, so nothing is finalized unless a
/// case asks for it.
const NOW_MS: i64 = 1_789_664_000_000;

/// A bucket plus the machine-side moves a run makes against it.
///
/// The store is shared so a fault hook can hold a second handle and run a
/// whole competing sync from inside another one's request.
#[derive(Clone)]
struct Bucket {
    store: Arc<MemoryStore>,
    keys: KeySpace,
}

impl Bucket {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            keys: KeySpace::new("ccusage/v1").expect("prefix"),
        }
    }

    /// One `ccusage sync run`, minus reading local logs: the shards are given.
    fn sync(&self, machine_id: &str, shards: Vec<Shard>) -> Result<Vec<String>, String> {
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

    fn input_tokens(&self) -> u64 {
        self.daily().totals().input_tokens
    }

    /// The totals a bucket with the same shards but no derived state would
    /// have: rollups thrown away and rebuilt in one pass.
    fn recomputed_input_tokens(&self) -> u64 {
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

    fn shard_keys(&self) -> Vec<String> {
        self.store
            .keys()
            .into_iter()
            .filter(|key| key.contains("/shards/"))
            .collect()
    }

    fn index_dates(&self, machine_id: &str) -> Vec<String> {
        machine::load_index(self.store.as_ref(), &self.keys, USER, machine_id)
            .expect("index")
            .shards
            .keys()
            .cloned()
            .collect()
    }
}

/// The invariant behind every row of the matrix: the incremental bucket agrees
/// with one recomputed from the shards alone.
fn assert_converged(bucket: &Bucket) {
    assert_eq!(
        bucket.input_tokens(),
        bucket.recomputed_input_tokens(),
        "incremental totals disagree with a clean recomputation"
    );
}

fn shard(machine_id: &str, date: &str, input: u64) -> Shard {
    shard_with_keys(machine_id, date, input, &[input])
}

fn shard_with_keys(machine_id: &str, date: &str, input: u64, dedupe: &[u64]) -> Shard {
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
