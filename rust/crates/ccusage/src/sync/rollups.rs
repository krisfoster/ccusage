//! Rebuilding the derived views in the bucket after shards change.
//!
//! The expensive part of a rollup is reading shards, so this reads as few as it
//! can: `daily.json` records the content hash every `(machine, agent, date)`
//! was last rolled up from, and a shard whose hash in the machine index still
//! matches is skipped entirely. A steady-state sync therefore reads one index
//! per machine and one shard — the day that changed.
//!
//! `daily.json` has as many writers as the user has machines, so it is written
//! with the generation it was read at and a lost race is re-run from the read
//! rather than overwritten. `weekly`, `monthly` and `models` are pure functions
//! of the daily rollup, so they are written unconditionally: the worst a lost
//! race can do is leave them one sync behind, which the next sync corrects,
//! whereas a compare-and-swap on four objects could leave them inconsistent
//! with each other.

use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition, RollupKind};
use ccusage_sync::duplicates::{KEY_INDEX_SCHEMA, KeyIndex, mark_duplicates};
use ccusage_sync::rollup::{ANOMALY_LATE_EDIT, Anomaly, Daily, ROLLUP_SCHEMA, ShardRef, derive};
use ccusage_sync::shard::{ParsedShard, Shard};
use serde::Serialize;

use super::bootstrap;
use super::machine;
use super::run::parse_date;

/// Bounded so a bucket under constant write pressure fails loudly rather than
/// spinning; one concurrent sync resolves on the first retry.
const MAX_ATTEMPTS: usize = 5;

pub(crate) type Result<T> = std::result::Result<T, String>;

/// What a rollup pass did, in the terms the user is told about.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RollupSummary {
    pub machines: usize,
    /// Shards read because their hash had changed.
    pub rolled_up: usize,
    /// Shards written by a newer ccusage than this one, left out of the totals.
    pub skipped_newer: usize,
    /// Shards an index promised that the bucket does not hold.
    pub missing: usize,
    /// Days that changed after they had settled, across every machine.
    pub late_edits: usize,
    /// Cells another machine had already reported, left out of the totals.
    pub suppressed_duplicates: usize,
}

impl RollupSummary {
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "Rolled up {} shard(s) from {} machine(s).",
            self.rolled_up, self.machines
        );
        if self.skipped_newer > 0 {
            text.push_str(&format!(
                " {} shard(s) were written by a newer ccusage and are not counted; upgrade to include them.",
                self.skipped_newer
            ));
        }
        if self.suppressed_duplicates > 0 {
            text.push_str(&format!(
                " {} cell(s) were reported by more than one machine and are counted once.",
                self.suppressed_duplicates
            ));
        }
        if self.late_edits > 0 {
            text.push_str(&format!(
                " {} finalized day(s) have been edited since they settled; the dashboard flags them.",
                self.late_edits
            ));
        }
        if self.missing > 0 {
            text.push_str(&format!(
                " {} shard(s) are listed by a machine but missing from the bucket; run 'ccusage sync run' on that machine.",
                self.missing
            ));
        }
        text
    }
}

/// Recomputes every rollup from whatever the bucket now holds.
pub(crate) fn refresh(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    now: &str,
) -> Result<RollupSummary> {
    for _ in 0..MAX_ATTEMPTS {
        let (mut daily, generation) = read_daily(store, keys)?;
        let mut key_index = read_key_index(store, keys)?;
        let machines = bootstrap::manifest_machines(store, keys)?;
        let mut summary = RollupSummary {
            machines: machines.len(),
            ..RollupSummary::default()
        };
        let mut live = Vec::new();
        let mut anomalies = Vec::new();

        for machine_id in &machines {
            let index = machine::load_index(store, keys, user_id, machine_id)?;
            for (entry_key, entry) in &index.shards {
                let Some((agent, utc_date)) = entry_key.split_once('/') else {
                    continue;
                };
                let reference = ShardRef {
                    machine_id: machine_id.clone(),
                    agent: agent.to_string(),
                    utc_date: utc_date.to_string(),
                };
                live.push(reference.key());
                if entry.late_edits > 0 {
                    summary.late_edits += 1;
                    anomalies.push(Anomaly {
                        kind: ANOMALY_LATE_EDIT.to_string(),
                        machine_id: machine_id.clone(),
                        agent: agent.to_string(),
                        utc_date: utc_date.to_string(),
                        detected_at: entry
                            .last_late_edit_at
                            .clone()
                            .unwrap_or_else(|| entry.updated_at.clone()),
                        detail: Some(format!(
                            "rewritten {} time(s) after the day settled",
                            entry.late_edits
                        )),
                    });
                }
                // The key index has to be re-read too when it is missing this
                // shard, or a bucket rolled up before dedupe existed would
                // never gain the keys that detect it.
                if daily.is_current(&reference, &entry.content_hash) && key_index.covers(&reference)
                {
                    continue;
                }
                match load_shard(store, keys, user_id, &reference)? {
                    Some(ParsedShard::Known(shard)) => {
                        daily.apply(&shard);
                        key_index.apply(&shard);
                        summary.rolled_up += 1;
                    }
                    // Counting a shard this build cannot fully read would
                    // under-report the day it covers, which is worse than
                    // leaving it out and saying so.
                    Some(ParsedShard::Newer { .. }) => summary.skipped_newer += 1,
                    None => summary.missing += 1,
                }
            }
        }

        // A shard that left the index — forgotten machine, pruned day — must
        // leave the totals too, or the dashboard keeps charging for it.
        for stale in stale_refs(&daily, &live) {
            daily.forget(&stale);
            key_index.forget(&stale);
        }

        // After every contribution is in place, never per shard: whether a cell
        // is a duplicate depends on what the other machines reported.
        mark_duplicates(&mut daily, &key_index);
        summary.suppressed_duplicates = daily
            .days
            .values()
            .flatten()
            .filter(|cell| cell.suppressed.is_some())
            .count();

        daily.schema = ROLLUP_SCHEMA;
        daily.generated_at = now.to_string();
        // Rebuilt, not appended to: an anomaly the machine indexes no longer
        // report is one the dashboard should stop showing.
        daily.anomalies = anomalies;

        match put(
            store,
            &keys.rollup(RollupKind::Daily),
            &daily,
            generation.as_deref(),
        ) {
            Ok(()) => {}
            Err(ObjectStoreError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }

        // Merge state rather than a view, and rebuildable from the shards, so
        // it rides along with the derived writes instead of its own CAS.
        key_index.schema = KEY_INDEX_SCHEMA;
        key_index.generated_at = now.to_string();
        put_derived(store, keys, RollupKind::Keys, &key_index)?;

        let derived = derive(&daily);
        put_derived(store, keys, RollupKind::Weekly, &derived.weekly)?;
        put_derived(store, keys, RollupKind::Monthly, &derived.monthly)?;
        put_derived(store, keys, RollupKind::Models, &derived.models)?;
        return Ok(summary);
    }
    Err(
        "the bucket's rollups kept changing under this sync. Re-run 'ccusage sync run' once no other sync is running."
            .to_string(),
    )
}

/// A key index this build cannot read is rebuilt rather than trusted: without
/// it the merge would under-suppress, never over-suppress.
fn read_key_index(store: &dyn ObjectStore, keys: &KeySpace) -> Result<KeyIndex> {
    let key = keys.rollup(RollupKind::Keys);
    let Some((body, _)) = store.get(&key).map_err(|error| error.to_string())? else {
        return Ok(KeyIndex::default());
    };
    let index: KeyIndex = match serde_json::from_slice(&body) {
        Ok(index) => index,
        Err(_) => return Ok(KeyIndex::default()),
    };
    if index.schema > KEY_INDEX_SCHEMA {
        return Ok(KeyIndex::default());
    }
    Ok(index)
}

fn stale_refs(daily: &Daily, live: &[String]) -> Vec<ShardRef> {
    daily
        .based_on
        .keys()
        .filter(|key| !live.contains(key))
        .filter_map(|key| {
            let mut parts = key.splitn(3, '/');
            Some(ShardRef {
                machine_id: parts.next()?.to_string(),
                agent: parts.next()?.to_string(),
                utc_date: parts.next()?.to_string(),
            })
        })
        .collect()
}

fn read_daily(store: &dyn ObjectStore, keys: &KeySpace) -> Result<(Daily, Option<String>)> {
    let key = keys.rollup(RollupKind::Daily);
    let Some((body, meta)) = store.get(&key).map_err(|error| error.to_string())? else {
        return Ok((Daily::default(), None));
    };
    let daily: Daily = serde_json::from_slice(&body)
        .map_err(|error| format!("{} is not readable: {error}", key.path()))?;
    // A rollup written by a newer ccusage is rebuilt from the shards rather
    // than half-read; the shards, not the rollup, are the source of truth.
    if daily.schema > ROLLUP_SCHEMA {
        return Ok((Daily::default(), meta.generation));
    }
    Ok((daily, meta.generation))
}

fn load_shard(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    reference: &ShardRef,
) -> Result<Option<ParsedShard>> {
    let date = parse_date(&reference.utc_date)?;
    let key = keys
        .shard(user_id, &reference.machine_id, &reference.agent, date)
        .map_err(|error| error.to_string())?;
    let Some((body, _)) = store.get(&key).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    Shard::parse(&body)
        .map(Some)
        .map_err(|error| format!("{} is not readable: {error}", key.path()))
}

fn put<T: Serialize>(
    store: &dyn ObjectStore,
    key: &Key,
    value: &T,
    generation: Option<&str>,
) -> std::result::Result<(), ObjectStoreError> {
    let body = serde_json::to_vec(value).expect("the rollup is serializable");
    let precondition = match generation {
        Some(generation) => Precondition::IfGenerationMatch(generation.to_string()),
        None => Precondition::IfAbsent,
    };
    store
        .put(key, &body, "application/json", &precondition)
        .map(|_| ())
}

fn put_derived<T: Serialize>(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    kind: RollupKind,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value).expect("the rollup is serializable");
    store
        .put(
            &keys.rollup(kind),
            &body,
            "application/json",
            &Precondition::None,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use ccusage_sync::rollup::{Models, Periodic};
    use ccusage_sync::shard::{Cell, Dedupe, DedupeKey, SHARD_SCHEMA};
    use ccusage_test_support::objectstore::MemoryStore;

    use super::super::machine::{IndexEntry, MachineIndex};
    use super::*;

    const USER: &str = "user-1";
    const NOW: &str = "2026-09-17T18:12:03Z";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    /// The same shard, but carrying the dedupe keys that make two machines'
    /// copies comparable.
    fn shard_with_keys(machine: &str, date: &str, input: u64, dedupe_keys: &[u64]) -> Shard {
        let mut shard = shard(machine, date, input);
        shard.cells[0].keys = dedupe_keys.iter().copied().map(DedupeKey).collect();
        shard.content_hash = None;
        shard.finish().expect("finish");
        shard
    }

    fn shard(machine: &str, date: &str, input: u64) -> Shard {
        let mut shard = Shard {
            schema: SHARD_SCHEMA,
            agent: "claude".to_string(),
            machine_id: machine.to_string(),
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
                ..Cell::default()
            }],
            sessions: Vec::new(),
            dedupe: Dedupe {
                algo: "sha256-64/v1".to_string(),
                salt: "salt:abcd".to_string(),
                count: 0,
            },
            content_hash: None,
        };
        shard.finish().expect("finish");
        shard
    }

    /// Writes a shard and the index entry that points at it, the way a run on
    /// that machine would have left the bucket.
    fn publish(store: &MemoryStore, keys: &KeySpace, shard: &Shard) {
        let date = parse_date(&shard.utc_date).expect("date");
        let key = keys
            .shard(USER, &shard.machine_id, &shard.agent, date)
            .expect("key");
        store
            .put(
                &key,
                &shard.to_json().expect("serialize"),
                "application/json",
                &Precondition::None,
            )
            .expect("put shard");
        machine::update_index(store, keys, USER, &shard.machine_id, |index| {
            index.shards.insert(
                MachineIndex::entry_key(&shard.agent, &shard.utc_date),
                IndexEntry {
                    content_hash: shard.content_hash.clone().expect("finished"),
                    finalized: false,
                    updated_at: NOW.to_string(),
                    ..IndexEntry::default()
                },
            );
        })
        .expect("index");
        register(store, keys, &shard.machine_id);
    }

    fn register(store: &MemoryStore, keys: &KeySpace, machine_id: &str) {
        bootstrap::ensure_manifest(store, keys, USER).expect("manifest");
        bootstrap::register_machine(store, keys, USER, machine_id).expect("register");
    }

    fn read_daily_object(store: &MemoryStore, keys: &KeySpace) -> Daily {
        let (body, _) = store
            .get(&keys.rollup(RollupKind::Daily))
            .expect("get")
            .expect("daily exists");
        serde_json::from_slice(&body).expect("parse")
    }

    fn read_models(store: &MemoryStore, keys: &KeySpace) -> Models {
        let (body, _) = store
            .get(&keys.rollup(RollupKind::Models))
            .expect("get")
            .expect("models exists");
        serde_json::from_slice(&body).expect("parse")
    }

    fn read_periodic(store: &MemoryStore, keys: &KeySpace, kind: RollupKind) -> Periodic {
        let (body, _) = store.get(&keys.rollup(kind)).expect("get").expect("exists");
        serde_json::from_slice(&body).expect("parse")
    }

    #[test]
    fn a_first_pass_rolls_up_every_machine_and_writes_all_four_objects() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("bbbb", "2026-09-17", 50));

        let summary = refresh(&store, &keys, USER, NOW).expect("refresh");

        assert_eq!(summary.rolled_up, 2);
        assert_eq!(summary.machines, 2);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 150);
        assert_eq!(
            read_models(&store, &keys).models["claude-sonnet-4-5"].input_tokens,
            150
        );
        assert_eq!(
            read_periodic(&store, &keys, RollupKind::Weekly).periods[0].period,
            "2026-W38"
        );
        assert_eq!(
            read_periodic(&store, &keys, RollupKind::Monthly).periods[0].period,
            "2026-09"
        );
    }

    /// The whole point of `basedOn`: a second pass over an unchanged bucket
    /// must not fetch a single shard.
    #[test]
    fn a_second_pass_over_unchanged_shards_reads_no_shards() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        refresh(&store, &keys, USER, NOW).expect("first");

        let summary = refresh(&store, &keys, USER, "2026-09-17T19:00:00Z").expect("second");

        assert_eq!(summary.rolled_up, 0);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 100);
    }

    #[test]
    fn only_the_shard_whose_hash_changed_is_re_read() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("aaaa", "2026-09-18", 100));
        refresh(&store, &keys, USER, NOW).expect("first");

        publish(&store, &keys, &shard("aaaa", "2026-09-18", 250));
        let summary = refresh(&store, &keys, USER, NOW).expect("second");

        assert_eq!(summary.rolled_up, 1);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 350);
    }

    /// Reading a shard this build does not understand as if it were a known one
    /// would silently under-report that day, so it is excluded and counted.
    #[test]
    fn a_shard_from_a_newer_ccusage_is_reported_rather_than_half_counted() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        let key = keys
            .shard(
                USER,
                "aaaa",
                "claude",
                parse_date("2026-09-17").expect("date"),
            )
            .expect("key");
        store
            .put(
                &key,
                br#"{"schema":99,"agent":"claude","buckets":[]}"#,
                "application/json",
                &Precondition::None,
            )
            .expect("overwrite");

        let summary = refresh(&store, &keys, USER, NOW).expect("refresh");

        assert_eq!(summary.skipped_newer, 1);
        assert_eq!(summary.rolled_up, 0);
        assert!(summary.to_text().contains("newer ccusage"));
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 0);
    }

    /// An interrupted run can leave an index entry ahead of its shard; that is
    /// a repair job, not a reason to abandon the other machines' rollups.
    #[test]
    fn a_shard_the_index_promises_but_the_bucket_lacks_is_counted_not_fatal() {
        let store = MemoryStore::new();
        let keys = keys();
        register(&store, &keys, "aaaa");
        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            index.shards.insert(
                MachineIndex::entry_key("claude", "2026-09-17"),
                IndexEntry {
                    content_hash: "sha256:missing".to_string(),
                    finalized: false,
                    updated_at: NOW.to_string(),
                    ..IndexEntry::default()
                },
            );
        })
        .expect("index");

        let summary = refresh(&store, &keys, USER, NOW).expect("refresh");

        assert_eq!(summary.missing, 1);
        assert!(summary.to_text().contains("missing from the bucket"));
    }

    /// Two machines reading the same synced log directory report the same
    /// entries; counting both would inflate the user's spend.
    #[test]
    fn the_same_entries_reported_by_two_machines_are_counted_once() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(
            &store,
            &keys,
            &shard_with_keys("aaaa", "2026-09-17", 100, &[1, 2]),
        );
        publish(
            &store,
            &keys,
            &shard_with_keys("bbbb", "2026-09-17", 100, &[1, 2]),
        );

        let summary = refresh(&store, &keys, USER, NOW).expect("refresh");

        // Both shards are rolled up; only one copy reaches the totals.
        assert_eq!(summary.rolled_up, 2);
        assert_eq!(summary.suppressed_duplicates, 1);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 100);
        assert_eq!(
            read_models(&store, &keys).models["claude-sonnet-4-5"].input_tokens,
            100
        );
    }

    /// The suppression has to survive a pass that re-reads nothing, which is
    /// what the separate key index exists for.
    #[test]
    fn a_duplicate_stays_suppressed_on_a_pass_that_reads_no_shards() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(
            &store,
            &keys,
            &shard_with_keys("aaaa", "2026-09-17", 100, &[1, 2]),
        );
        publish(
            &store,
            &keys,
            &shard_with_keys("bbbb", "2026-09-17", 100, &[1, 2]),
        );
        refresh(&store, &keys, USER, NOW).expect("first");

        let summary = refresh(&store, &keys, USER, "2026-09-17T19:00:00Z").expect("second");

        assert_eq!(summary.rolled_up, 0);
        assert_eq!(summary.suppressed_duplicates, 1);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 100);
    }

    /// The dashboard reads anomalies off the daily rollup, so a day a machine
    /// rewrote after it settled has to travel from that machine's index into
    /// `daily.json` — the reader never sees the index itself.
    #[test]
    fn a_late_edit_recorded_by_a_machine_reaches_the_daily_rollup() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            let entry = index
                .shards
                .get_mut(&MachineIndex::entry_key("claude", "2026-09-17"))
                .expect("entry");
            entry.finalized = true;
            entry.late_edits = 2;
            entry.last_late_edit_at = Some("2026-09-20T08:00:00Z".to_string());
        })
        .expect("index");

        let summary = refresh(&store, &keys, USER, NOW).expect("refresh");

        assert_eq!(summary.late_edits, 1);
        let anomalies = read_daily_object(&store, &keys).anomalies;
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].kind, ANOMALY_LATE_EDIT);
        assert_eq!(anomalies[0].utc_date, "2026-09-17");
        assert_eq!(anomalies[0].detected_at, "2026-09-20T08:00:00Z");
    }

    /// A machine whose late edit has been cleared should stop being flagged,
    /// so anomalies are rebuilt each pass rather than accumulated.
    #[test]
    fn an_anomaly_that_no_longer_exists_is_dropped_on_the_next_pass() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            index
                .shards
                .get_mut(&MachineIndex::entry_key("claude", "2026-09-17"))
                .expect("entry")
                .late_edits = 1;
        })
        .expect("index");
        refresh(&store, &keys, USER, NOW).expect("first");
        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            index
                .shards
                .get_mut(&MachineIndex::entry_key("claude", "2026-09-17"))
                .expect("entry")
                .late_edits = 0;
        })
        .expect("index");

        let summary = refresh(&store, &keys, USER, "2026-09-17T20:00:00Z").expect("second");

        assert_eq!(summary.late_edits, 0);
        assert!(read_daily_object(&store, &keys).anomalies.is_empty());
    }

    /// A day removed from a machine's index has to leave the totals, or the
    /// dashboard keeps reporting spend the user has deleted.
    #[test]
    fn a_day_dropped_from_the_index_is_dropped_from_the_totals() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("aaaa", "2026-09-18", 40));
        refresh(&store, &keys, USER, NOW).expect("first");

        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            index
                .shards
                .remove(&MachineIndex::entry_key("claude", "2026-09-18"));
        })
        .expect("index");
        refresh(&store, &keys, USER, NOW).expect("second");

        let daily = read_daily_object(&store, &keys);
        assert_eq!(daily.totals().input_tokens, 100);
        assert!(!daily.days.contains_key("2026-09-18"));
        assert!(!daily.based_on.contains_key("aaaa/claude/2026-09-18"));
    }

    /// A machine that joins later contributes without the earlier machines
    /// re-reading anything they had already rolled up.
    #[test]
    fn a_machine_that_joins_later_is_merged_into_the_existing_rollup() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        refresh(&store, &keys, USER, NOW).expect("first");

        publish(&store, &keys, &shard("bbbb", "2026-09-17", 25));
        let summary = refresh(&store, &keys, USER, NOW).expect("second");

        assert_eq!(summary.rolled_up, 1);
        assert_eq!(summary.machines, 2);
        assert_eq!(read_daily_object(&store, &keys).totals().input_tokens, 125);
    }

    /// The views are spend and the key index is salted dedupe material, so
    /// none of them may land in the public dashboard prefix.
    #[test]
    fn every_rollup_object_is_written_to_a_private_key() {
        for kind in [
            RollupKind::Daily,
            RollupKind::Weekly,
            RollupKind::Monthly,
            RollupKind::Models,
            RollupKind::Keys,
        ] {
            assert!(!keys().rollup(kind).is_public());
        }
    }
}
