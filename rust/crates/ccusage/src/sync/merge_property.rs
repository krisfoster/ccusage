//! §4.5 of `specs/sync-merge-test-plan.md`: a seeded driver over the merge
//! alphabet — add a day, edit one, delete one locally, sync a machine, sync a
//! machine through an injected fault, refresh the rollups, repair — checking
//! the invariants after every single step rather than only at the end.
//!
//! The point of this layer is the interleavings nobody enumerated. The
//! matrices in `merge_matrix.rs` each pin one boundary; this walks thousands
//! of boundary sequences and asserts the same handful of properties over all
//! of them. Seeds are fixed, so a failure is a reproducible test case.

use std::collections::BTreeMap;

use ccusage_sync::shard::Shard;
use ccusage_test_support::objectstore::{Fault, When};

use super::machine;
use super::merge_matrix::{Bucket, USER, shard_with_keys};

const MACHINES: [&str; 2] = ["aaaa", "bbbb"];
const DATES: [&str; 5] = [
    "2026-09-10",
    "2026-09-11",
    "2026-09-12",
    "2026-09-13",
    "2026-09-14",
];

/// Reproducible and cheap; the sequences are what matters here, not the
/// statistical quality of the stream.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len() as u64) as usize]
    }
}

/// What each machine believes it has locally, and what the bucket last
/// accepted from it. The bucket is never asked to agree with the local state
/// directly — a day deleted locally stays in the bucket by design — only that
/// every day it holds is one some machine actually reported.
#[derive(Default)]
struct Model {
    local: BTreeMap<(String, String), u64>,
    reported: BTreeMap<(String, String), Vec<u64>>,
}

impl Model {
    fn set(&mut self, machine_id: &str, date: &str, input: u64) {
        let cell = (machine_id.to_string(), date.to_string());
        self.local.insert(cell.clone(), input);
        self.reported.entry(cell).or_default().push(input);
    }

    fn remove(&mut self, machine_id: &str, date: &str) {
        self.local
            .remove(&(machine_id.to_string(), date.to_string()));
    }

    fn days_of(&self, machine_id: &str) -> Vec<(String, u64)> {
        self.local
            .iter()
            .filter(|((machine, _), _)| machine == machine_id)
            .map(|((_, date), input)| (date.clone(), *input))
            .collect()
    }

    /// A value the bucket is allowed to be holding for that day: something the
    /// machine said at some point, never a number nobody reported.
    fn ever_reported(&self, machine_id: &str, date: &str, input: u64) -> bool {
        self.reported
            .get(&(machine_id.to_string(), date.to_string()))
            .is_some_and(|seen| seen.contains(&input))
    }
}

/// Dedupe keys are unique per machine and day, so no cross-machine
/// suppression is in play: the driver is checking merge mechanics, and
/// suppression has its own dedicated tests.
fn shard_for(machine_id: &str, date: &str, input: u64) -> Shard {
    let machine = MACHINES.iter().position(|m| *m == machine_id).unwrap_or(0) as u64;
    let day = DATES.iter().position(|d| *d == date).unwrap_or(0) as u64;
    shard_with_keys(
        machine_id,
        date,
        input,
        &[1_000_000 * (machine + 1) + 1_000 * day],
    )
}

/// The `(machine, "agent/date")` pairs the bucket physically holds a shard
/// object for, read from the keys rather than the bodies.
fn stored_shards(bucket: &Bucket) -> Vec<(String, String)> {
    bucket
        .shard_keys()
        .into_iter()
        .filter_map(|key| {
            let (machine_part, rest) = key.split_once("/shards/")?;
            let machine_id = machine_part.rsplit_once('/')?.1.to_string();
            let mut parts = rest.trim_end_matches(".json").split('/');
            let agent = parts.next()?;
            let year = parts.next()?;
            let month = parts.next()?;
            let day = parts.next()?;
            Some((machine_id, format!("{agent}/{year}-{month}-{day}")))
        })
        .collect()
}

fn shard_body(bucket: &Bucket, machine_id: &str, entry: &str) -> Option<Shard> {
    let (agent, date) = entry.split_once('/').expect("agent/date");
    let (year, rest) = date.split_once('-').expect("year");
    let (month, day) = rest.split_once('-').expect("month");
    let key = format!(
        "ccusage/v1/users/{USER}/machines/{machine_id}/shards/{agent}/{year}/{month}/{day}.json"
    );
    let body = bucket.store.body(&key)?;
    Some(serde_json::from_slice::<Shard>(&body).expect("parse shard"))
}

fn input_tokens_of(bucket: &Bucket, machine_id: &str, entry: &str) -> u64 {
    shard_body(bucket, machine_id, entry)
        .expect("the shard object")
        .cells
        .iter()
        .map(|cell| cell.input_tokens)
        .sum()
}

fn content_hash_of(bucket: &Bucket, machine_id: &str, entry: &str) -> Option<String> {
    shard_body(bucket, machine_id, entry)?.content_hash
}

/// Whether every index entry names an object whose content is the content the
/// entry promises.
///
/// A run that dies after its shard upload and before its index update leaves
/// an object ahead of the hash the index carries. The index is the authority,
/// so the incremental rollups follow it and a pass that reads every body does
/// not — the two are entitled to disagree until the machine uploads that day
/// again or a repair re-reads the bodies. It is the only licence for a
/// conservation gap, so the driver derives it rather than guessing from which
/// step last failed.
fn index_matches_objects(bucket: &Bucket) -> bool {
    MACHINES.iter().all(|machine_id| {
        let index = machine::load_index(bucket.store.as_ref(), &bucket.keys, USER, machine_id)
            .expect("the machine index");
        index.shards.iter().all(|(entry, promise)| {
            content_hash_of(bucket, machine_id, entry).as_deref() == Some(&promise.content_hash)
        })
    })
}

/// Invariants 1, 2, 3 and 7 of §4.4, checked after every step.
///
/// `unsettled` says the derived views are a generation behind a run that died
/// before its rollup pass, which is the one lag §4.4 invariant 4 permits.
/// Conservation is asserted exactly on every other step.
fn assert_invariants(bucket: &Bucket, model: &Model, unsettled: bool, log: &Log) {
    let settled = !unsettled && index_matches_objects(bucket);
    let stored = stored_shards(bucket);

    for machine_id in MACHINES {
        for entry in bucket.index_dates(machine_id) {
            // 2: no dangling promise. The index is written after the object
            // it names and cleared before the object goes, so an entry with
            // nothing behind it is a bug rather than a state to recover from.
            assert!(
                stored.contains(&(machine_id.to_string(), entry.clone())),
                "{log}{machine_id} promises {entry} and the object is not there"
            );
            // 7: no fabricated usage. Whatever the bucket holds for a day has
            // to be a number that machine reported at some point.
            let held = input_tokens_of(bucket, machine_id, &entry);
            let date = entry.split_once('/').expect("agent/date").1;
            assert!(
                model.ever_reported(machine_id, date, held),
                "{log}{machine_id}/{date} holds {held}, which no machine ever reported"
            );
        }
    }

    // 1: conservation. The incremental rollups have to agree with a rollup
    // built from the shards alone.
    if settled && bucket.store.body("ccusage/v1/rollup/daily.json").is_some() {
        assert_eq!(
            bucket.input_tokens(),
            bucket.recomputed_input_tokens(),
            "{log}incremental totals disagree with a clean recomputation"
        );
    }

    // 3: no invisible usage. An orphan object — uploaded by a run that died
    // before its index update — is allowed, but only until a repair, which
    // has to adopt every one of them.
    let indexed: Vec<(String, String)> = MACHINES
        .iter()
        .flat_map(|machine_id| {
            bucket
                .index_dates(machine_id)
                .into_iter()
                .map(move |entry| (machine_id.to_string(), entry))
        })
        .collect();
    for orphan in stored.iter().filter(|shard| !indexed.contains(shard)) {
        assert!(
            MACHINES.contains(&orphan.0.as_str()),
            "{log}an object under an unknown machine: {orphan:?}"
        );
    }
}

/// Nothing a failure says may carry the bucket's salt, its user id or a piece
/// of an object body (§4.4 invariant 8).
fn assert_private(error: &str, log: &Log) {
    for secret in ["salt:abcd", "sha256-64/v1", USER] {
        assert!(
            !error.contains(secret),
            "{log}the error names {secret}: {error}"
        );
    }
}

/// The steps taken so far, so a failing seed prints the sequence that got
/// there rather than only the seed. Rendered lazily, inside the assertion
/// messages, so a passing run pays nothing for it.
struct Log {
    seed: u64,
    steps: Vec<String>,
}

impl std::fmt::Display for Log {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "seed {}", self.seed)?;
        for (number, step) in self.steps.iter().enumerate() {
            writeln!(formatter, "  {number}: {step}")?;
        }
        Ok(())
    }
}

/// One fault at one boundary of the write path, armed for the next run only.
fn arm_fault(bucket: &Bucket, choice: u64) {
    match choice % 6 {
        0 => bucket.store.fail_on(When::put("/shards/"), Fault::Network),
        1 => bucket
            .store
            .lose_response_on(When::put("/shards/"), Fault::Network),
        2 => bucket
            .store
            .fail_on(When::put("index.json"), Fault::Server { status: 503 }),
        3 => bucket
            .store
            .lose_response_on(When::put("index.json"), Fault::Network),
        4 => bucket
            .store
            .fail_on(When::put("machine.json"), Fault::Network),
        _ => bucket
            .store
            .fail_on(When::put("daily.json").times(5), Fault::Conflict),
    }
}

fn run_sequence(seed: u64, steps: usize) {
    let bucket = Bucket::new();
    let mut model = Model::default();
    let mut rng = Rng(seed);
    // Set when a run dies before its rollup pass, so the derived views are a
    // generation behind the shards; any completed pass catches them up.
    let mut derived_behind = false;
    let mut log = Log {
        seed,
        steps: Vec::new(),
    };

    for _ in 0..steps {
        let machine_id = *rng.pick(&MACHINES);
        let date = *rng.pick(&DATES);

        match rng.below(10) {
            // Local usage appears or is rewritten.
            0..=2 => {
                let input = 25 + rng.below(20) * 25;
                log.steps
                    .push(format!("{machine_id} logs {date} = {input}"));
                model.set(machine_id, date, input);
            }
            // The local logs lose a day: rotated away, or a profile moved.
            3 => {
                log.steps.push(format!("{machine_id} loses {date}"));
                model.remove(machine_id, date);
            }
            // A clean run.
            4..=6 => {
                log.steps.push(format!("{machine_id} syncs"));
                let shards = model
                    .days_of(machine_id)
                    .into_iter()
                    .map(|(date, input)| shard_for(machine_id, &date, input))
                    .collect();
                if let Err(error) = bucket.sync(machine_id, shards) {
                    assert_private(&error, &log);
                    panic!("{log}a run with no fault armed failed: {error}");
                }
                // Invariant: a clean run leaves nothing of that machine's
                // local state out of the bucket.
                for (date, input) in model.days_of(machine_id) {
                    let entry = format!("claude/{date}");
                    assert!(
                        bucket.index_dates(machine_id).contains(&entry),
                        "{log}{machine_id} synced cleanly and {date} is missing"
                    );
                    assert_eq!(
                        input_tokens_of(&bucket, machine_id, &entry),
                        input,
                        "{log}{machine_id}/{date} did not take the local content"
                    );
                }
                derived_behind = false;
            }
            // A run that meets a fault somewhere in the write path.
            7..=8 => {
                let choice = rng.next();
                log.steps
                    .push(format!("{machine_id} syncs into fault {}", choice % 6));
                arm_fault(&bucket, choice);
                let shards = model
                    .days_of(machine_id)
                    .into_iter()
                    .map(|(date, input)| shard_for(machine_id, &date, input))
                    .collect();
                match bucket.sync(machine_id, shards) {
                    Ok(_) => derived_behind = false,
                    Err(error) => {
                        assert_private(&error, &log);
                        derived_behind = true;
                    }
                }
                bucket.store.clear_rules();
            }
            // Housekeeping: a rollup pass, or a repair.
            _ => {
                derived_behind = false;
                if rng.below(2) == 0 {
                    log.steps.push("rollups refresh".to_string());
                    bucket.refresh().expect("a refresh with no fault armed");
                } else {
                    log.steps.push("repair".to_string());
                    bucket.repair().expect("a repair with no fault armed");
                    // Invariant 3: a repair leaves no orphan behind.
                    let indexed: Vec<(String, String)> = MACHINES
                        .iter()
                        .flat_map(|machine_id| {
                            bucket
                                .index_dates(machine_id)
                                .into_iter()
                                .map(move |entry| (machine_id.to_string(), entry))
                        })
                        .collect();
                    for shard in stored_shards(&bucket) {
                        assert!(
                            indexed.contains(&shard),
                            "{log}repair left {shard:?} invisible"
                        );
                    }
                }
            }
        }

        assert_invariants(&bucket, &model, derived_behind, &log);
    }

    // Whatever the sequence did, a repair and a rollup pass have to land on
    // the same totals as a bucket rebuilt from the shards alone, and running
    // them again has to change nothing but the timestamps.
    bucket.repair().expect("closing repair");
    bucket.refresh().expect("closing refresh");
    log.steps.push("closing repair and refresh".to_string());
    assert_invariants(&bucket, &model, false, &log);

    let before = shard_bodies(&bucket);
    bucket.refresh().expect("idempotent refresh");
    assert_eq!(
        shard_bodies(&bucket),
        before,
        "seed {seed}: a second pass rewrote the shards"
    );
    assert_eq!(
        bucket.input_tokens(),
        bucket.recomputed_input_tokens(),
        "seed {seed}: the closing totals disagree with a clean recomputation"
    );
}

fn shard_bodies(bucket: &Bucket) -> BTreeMap<String, Vec<u8>> {
    bucket
        .shard_keys()
        .into_iter()
        .map(|key| {
            let body = bucket.store.body(&key).expect("shard body");
            (key, body)
        })
        .collect()
}

#[test]
fn seeded_sequences_of_merges_faults_and_repairs_hold_the_invariants() {
    for seed in 1..=64_u64 {
        run_sequence(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 40);
    }
}

/// The driver is only worth anything if its model of the bucket is checked
/// against something independent, so: the machine index and the objects say
/// the same thing about every day, at rest.
#[test]
fn the_driver_leaves_a_bucket_whose_index_and_objects_agree() {
    let bucket = Bucket::new();
    bucket
        .sync("aaaa", vec![shard_for("aaaa", "2026-09-10", 100)])
        .expect("sync");

    let index = machine::load_index(bucket.store.as_ref(), &bucket.keys, USER, "aaaa")
        .expect("index")
        .shards;

    assert_eq!(index.len(), 1);
    assert_eq!(stored_shards(&bucket).len(), 1);
    assert_eq!(
        input_tokens_of(&bucket, "aaaa", index.keys().next().expect("entry")),
        100
    );
}
