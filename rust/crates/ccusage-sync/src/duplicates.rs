//! Finding the same usage reported by two machines.
//!
//! Two machines reading the same synced log directory both report those
//! entries, and counting them twice inflates the user's spend. The dedupe keys
//! that make this detectable are far bigger than the rollup they protect — a
//! busy day is thousands of keys against a few hundred cells — and nothing but
//! this merge reads them, so they live in their own private object rather than
//! inside `daily.json`.
//!
//! The rules are deliberately conservative, because over-counting is visible
//! and correctable while silently deleting usage is neither:
//!
//! - disjoint keys: two machines genuinely both worked in that window, count
//!   both;
//! - one cell's keys wholly contained in another's: the same entries, suppress
//!   the copy on the lexicographically larger machine ID so every machine
//!   independently reaches the same answer;
//! - partial overlap: suppress nothing and record the overlap, because the
//!   non-overlapping part is real usage that a whole-cell subtraction would
//!   destroy.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::rollup::{Daily, DailyCell, ShardRef};
use crate::shard::{DedupeKey, Shard};

pub const KEY_INDEX_SCHEMA: u32 = 1;

/// Marks a cell whose entries another machine already reported.
pub const SUPPRESSED_DUPLICATE: &str = "duplicateSuppressed";

/// Prefix of the advisory note left on a partially overlapping cell. The note
/// carries the overlap as a percentage, e.g. `duplicateSuspected:40%`.
pub const NOTE_DUPLICATE_SUSPECTED: &str = "duplicateSuspected";

/// The dedupe keys of one cell.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CellKeys {
    #[serde(rename = "i")]
    pub bucket: u16,
    #[serde(rename = "m")]
    pub model: String,
    #[serde(rename = "k", default)]
    pub keys: Vec<DedupeKey>,
}

/// One shard's keys, with the salt they were derived under.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardKeys {
    /// Identifier of the salt, never the salt itself. Keys made under
    /// different salts are incomparable, so they are never intersected.
    pub salt: String,
    #[serde(default)]
    pub cells: Vec<CellKeys>,
}

/// `rollup/keys.json`: the dedupe keys behind the daily rollup, kept so an
/// incremental pass can compare a changed shard against machines it did not
/// re-read.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyIndex {
    pub schema: u32,
    pub generated_at: String,
    /// `machineId/agent/utcDate` → that shard's keys.
    #[serde(default)]
    pub shards: BTreeMap<String, ShardKeys>,
}

impl Default for KeyIndex {
    fn default() -> Self {
        Self {
            schema: KEY_INDEX_SCHEMA,
            generated_at: String::new(),
            shards: BTreeMap::new(),
        }
    }
}

impl KeyIndex {
    /// Whether this shard's keys are already held, keys or not.
    ///
    /// A shard with no keys still gets an entry, so "already covered" and
    /// "never read" stay distinguishable and a bucket that predates this index
    /// converges after one pass instead of re-reading forever.
    pub fn covers(&self, reference: &ShardRef) -> bool {
        self.shards.contains_key(&reference.key())
    }

    /// Replaces everything this shard contributed.
    pub fn apply(&mut self, shard: &Shard) {
        let cells = shard
            .cells
            .iter()
            .filter(|cell| !cell.keys.is_empty())
            .map(|cell| CellKeys {
                bucket: cell.bucket,
                model: cell.model.clone(),
                keys: cell.keys.clone(),
            })
            .collect::<Vec<_>>();
        let reference = ShardRef::of(shard);
        self.shards.insert(
            reference.key(),
            ShardKeys {
                salt: shard.dedupe.salt.clone(),
                cells,
            },
        );
    }

    pub fn forget(&mut self, reference: &ShardRef) {
        self.shards.remove(&reference.key());
    }

    pub fn forget_machine(&mut self, machine_id: &str) {
        self.shards
            .retain(|key, _| !key.starts_with(&format!("{machine_id}/")));
    }

    fn cell_keys(
        &self,
        machine_id: &str,
        agent: &str,
        utc_date: &str,
        bucket: u16,
        model: &str,
    ) -> Option<(&str, &[DedupeKey])> {
        let shard = self
            .shards
            .get(&format!("{machine_id}/{agent}/{utc_date}"))?;
        let cell = shard
            .cells
            .iter()
            .find(|cell| cell.bucket == bucket && cell.model == model)?;
        Some((shard.salt.as_str(), &cell.keys))
    }
}

/// Recomputes every duplicate verdict on the daily rollup from the key index.
///
/// Rebuilt rather than accumulated: a machine that stops reporting a day must
/// stop suppressing another machine's copy of it.
pub fn mark_duplicates(daily: &mut Daily, keys: &KeyIndex) {
    for (utc_date, cells) in &mut daily.days {
        for cell in cells.iter_mut() {
            cell.suppressed = None;
            cell.notes
                .retain(|note| !note.starts_with(NOTE_DUPLICATE_SUSPECTED));
        }
        // Cells are sorted by (bucket, machine, agent, model), so a group of
        // candidates is contiguous; collecting indexes keeps the borrow simple.
        let groups = group_indexes(cells);
        for group in groups {
            resolve_group(utc_date, cells, &group, keys);
        }
    }
}

/// Indexes of cells sharing an (agent, bucket, model) across machines.
fn group_indexes(cells: &[DailyCell]) -> Vec<Vec<usize>> {
    let mut groups: BTreeMap<(u16, &str, &str), Vec<usize>> = BTreeMap::new();
    for (index, cell) in cells.iter().enumerate() {
        groups
            .entry((cell.bucket, cell.agent.as_str(), cell.model.as_str()))
            .or_default()
            .push(index);
    }
    groups
        .into_values()
        .filter(|group| group.len() > 1)
        .collect()
}

fn resolve_group(utc_date: &str, cells: &mut [DailyCell], group: &[usize], keys: &KeyIndex) {
    let mut ordered = group.to_vec();
    ordered.sort_by(|left, right| cells[*left].machine_id.cmp(&cells[*right].machine_id));

    // Every machine before this one in the ordering is treated as the owner of
    // the keys it reported, so the verdict does not depend on which machine
    // happens to be running the rollup.
    let mut claimed: BTreeMap<&str, BTreeSet<DedupeKey>> = BTreeMap::new();
    for index in ordered {
        let cell = &cells[index];
        let Some((salt, cell_keys)) = keys.cell_keys(
            &cell.machine_id,
            &cell.agent,
            utc_date,
            cell.bucket,
            &cell.model,
        ) else {
            continue;
        };
        if cell_keys.is_empty() {
            continue;
        }
        let owned = claimed.entry(salt).or_default();
        let overlap = cell_keys.iter().filter(|key| owned.contains(key)).count();
        let verdict = if overlap == cell_keys.len() {
            Some(true)
        } else if overlap > 0 {
            Some(false)
        } else {
            None
        };
        let ratio = percent(overlap, cell_keys.len());
        owned.extend(cell_keys.iter().copied());

        let cell = &mut cells[index];
        match verdict {
            Some(true) => cell.suppressed = Some(SUPPRESSED_DUPLICATE.to_string()),
            Some(false) => cell
                .notes
                .push(format!("{NOTE_DUPLICATE_SUSPECTED}:{ratio}%")),
            None => {}
        }
    }
}

fn percent(part: usize, whole: usize) -> u32 {
    if whole == 0 {
        return 0;
    }
    u32::try_from(part * 100 / whole).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::{Cell, Dedupe, Shard};

    const NOW: &str = "2026-09-17T18:12:03Z";
    const DATE: &str = "2026-09-17";

    fn shard(machine: &str, cells: Vec<Cell>) -> Shard {
        let mut shard = Shard {
            schema: crate::shard::SHARD_SCHEMA,
            agent: "claude".to_string(),
            machine_id: machine.to_string(),
            user_id: "user-1".to_string(),
            utc_date: DATE.to_string(),
            generated_at: NOW.to_string(),
            ccusage_version: "20.0.21".to_string(),
            cost_mode: "auto".to_string(),
            pricing_snapshot: "litellm@2026-09-16".to_string(),
            cells,
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

    fn cell(keys: &[u64]) -> Cell {
        Cell {
            bucket: 40,
            model: "claude-sonnet-4-5".to_string(),
            input_tokens: 100,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            cost: 1.0,
            messages: u32::try_from(keys.len()).expect("small"),
            keys: keys.iter().copied().map(DedupeKey).collect(),
        }
    }

    fn merge(shards: &[Shard]) -> (Daily, KeyIndex) {
        let mut daily = Daily::default();
        let mut keys = KeyIndex::default();
        for shard in shards {
            daily.apply(shard);
            keys.apply(shard);
        }
        mark_duplicates(&mut daily, &keys);
        (daily, keys)
    }

    /// Two machines working at the same time on different things is normal;
    /// suppressing either would under-report real spend.
    #[test]
    fn two_machines_with_different_entries_are_both_counted() {
        let (daily, _) = merge(&[
            shard("aaaa", vec![cell(&[1, 2, 3])]),
            shard("bbbb", vec![cell(&[4, 5, 6])]),
        ]);

        assert_eq!(daily.totals().input_tokens, 200);
        assert!(
            daily.days[DATE]
                .iter()
                .all(|cell| cell.suppressed.is_none())
        );
    }

    #[test]
    fn the_same_entries_seen_twice_are_counted_once() {
        let (daily, _) = merge(&[
            shard("aaaa", vec![cell(&[1, 2, 3])]),
            shard("bbbb", vec![cell(&[1, 2, 3])]),
        ]);

        assert_eq!(daily.totals().input_tokens, 100);
        let suppressed: Vec<&DailyCell> = daily.days[DATE]
            .iter()
            .filter(|cell| cell.suppressed.is_some())
            .collect();
        assert_eq!(suppressed.len(), 1);
        // Lexicographically larger machine yields, so both machines agree.
        assert_eq!(suppressed[0].machine_id, "bbbb");
    }

    /// The overlapping part is real on one machine and the rest is real on the
    /// other; dropping the whole cell would delete usage that happened.
    #[test]
    fn a_partial_overlap_suppresses_nothing_and_is_flagged() {
        let (daily, _) = merge(&[
            shard("aaaa", vec![cell(&[1, 2, 3, 4])]),
            shard("bbbb", vec![cell(&[3, 4, 5, 6])]),
        ]);

        assert_eq!(daily.totals().input_tokens, 200);
        let flagged = daily.days[DATE]
            .iter()
            .find(|cell| cell.machine_id == "bbbb")
            .expect("cell");
        assert!(flagged.suppressed.is_none());
        assert_eq!(flagged.notes, vec!["duplicateSuspected:50%".to_string()]);
    }

    /// A shard whose keys were made under a different salt cannot be compared
    /// with this bucket's, and guessing would suppress unrelated usage.
    #[test]
    fn keys_made_under_another_salt_are_never_intersected() {
        let mut other = shard("bbbb", vec![cell(&[1, 2, 3])]);
        other.dedupe.salt = "salt:9999".to_string();
        let (daily, _) = merge(&[shard("aaaa", vec![cell(&[1, 2, 3])]), other]);

        assert_eq!(daily.totals().input_tokens, 200);
    }

    /// The verdicts are derived state: once the other machine stops reporting
    /// the day, the surviving copy has to be counted again.
    #[test]
    fn a_suppressed_cell_is_restored_when_the_other_machine_forgets_the_day() {
        let first = shard("aaaa", vec![cell(&[1, 2, 3])]);
        let second = shard("bbbb", vec![cell(&[1, 2, 3])]);
        let (mut daily, mut keys) = merge(&[first.clone(), second]);
        assert_eq!(daily.totals().input_tokens, 100);

        let reference = ShardRef::of(&first);
        daily.forget(&reference);
        keys.forget(&reference);
        mark_duplicates(&mut daily, &keys);

        assert_eq!(daily.totals().input_tokens, 100);
        assert!(
            daily.days[DATE]
                .iter()
                .all(|cell| cell.suppressed.is_none())
        );
    }

    /// Three machines reporting the same entries must leave exactly one copy,
    /// not suppress every cell after the first comparison.
    #[test]
    fn three_copies_of_the_same_entries_leave_one() {
        let (daily, _) = merge(&[
            shard("aaaa", vec![cell(&[1, 2])]),
            shard("bbbb", vec![cell(&[1, 2])]),
            shard("cccc", vec![cell(&[1, 2])]),
        ]);

        assert_eq!(daily.totals().input_tokens, 100);
        assert_eq!(
            daily.days[DATE]
                .iter()
                .filter(|cell| cell.suppressed.is_some())
                .count(),
            2
        );
    }

    #[test]
    fn forgetting_a_machine_drops_its_keys() {
        let (_, mut keys) = merge(&[
            shard("aaaa", vec![cell(&[1, 2])]),
            shard("bbbb", vec![cell(&[3])]),
        ]);

        keys.forget_machine("aaaa");

        assert_eq!(keys.shards.len(), 1);
        assert!(keys.shards.contains_key("bbbb/claude/2026-09-17"));
    }
}
