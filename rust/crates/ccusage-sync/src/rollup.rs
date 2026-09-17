//! Derived views of the shards: what the dashboard actually loads.
//!
//! `daily.json` is the only rollup with real content; `weekly.json`,
//! `monthly.json` and `models.json` are pure functions of it. That is deliberate
//! — a derived file that is *not* derived from a single source drifts, and
//! drift in a spend report is indistinguishable from a bug in the fold.
//!
//! The daily rollup keeps the 15-minute buckets rather than day totals, because
//! a viewer in `+05:45` has to re-cut the day locally; and it keeps every cell
//! tagged with the machine and agent that produced it, so a filter in the
//! dashboard is a projection rather than a re-read of every shard.
//!
//! Incremental updates work because a cell's `(machine, agent, date)` triple is
//! exactly a shard's identity: applying a shard replaces that shard's cells and
//! nothing else, so the result cannot depend on how many times, or in what
//! order, shards were applied. `based_on` records the content hash each triple
//! was last rolled up from, which is what lets a sync skip fetching the shards
//! it already accounted for.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::shard::Shard;

/// Bumped when a reader written against the current shape would misread a
/// rollup. The dashboard checks this before trusting the file.
pub const ROLLUP_SCHEMA: u32 = 1;

/// Which shard a set of cells came from.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ShardRef {
    pub machine_id: String,
    pub agent: String,
    pub utc_date: String,
}

impl ShardRef {
    pub fn of(shard: &Shard) -> Self {
        Self {
            machine_id: shard.machine_id.clone(),
            agent: shard.agent.clone(),
            utc_date: shard.utc_date.clone(),
        }
    }

    /// The `based_on` key. Machine IDs and agents contain no `/`, so the join is
    /// unambiguous.
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.machine_id, self.agent, self.utc_date)
    }
}

/// Totals that every rollup reports, in the same shape everywhere so the
/// dashboard has one summing routine.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub messages: u64,
}

impl Totals {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_write_tokens + self.cache_read_tokens
    }

    fn add(&mut self, cell: &DailyCell) {
        self.input_tokens += cell.input_tokens;
        self.output_tokens += cell.output_tokens;
        self.cache_write_tokens += cell.cache_write_tokens;
        self.cache_read_tokens += cell.cache_read_tokens;
        self.cost += cell.cost;
        self.messages += u64::from(cell.messages);
    }
}

/// One model's usage in one 15-minute UTC bucket, tagged with its origin.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct DailyCell {
    #[serde(rename = "machine")]
    pub machine_id: String,
    pub agent: String,
    /// Bucket index within the UTC day, `0..96`.
    #[serde(rename = "i")]
    pub bucket: u16,
    #[serde(rename = "m")]
    pub model: String,
    #[serde(rename = "in", default, skip_serializing_if = "is_zero")]
    pub input_tokens: u64,
    #[serde(rename = "out", default, skip_serializing_if = "is_zero")]
    pub output_tokens: u64,
    #[serde(rename = "cw", default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
    #[serde(rename = "cr", default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u64,
    #[serde(rename = "cost", default)]
    pub cost: f64,
    #[serde(rename = "msgs", default, skip_serializing_if = "is_zero_u32")]
    pub messages: u32,
    /// Why this cell is not counted in the totals, when it is not. Empty on the
    /// normal path; `duplicateSuppressed` when another machine provably
    /// reported the same entries (see the cross-machine merge rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<String>,
    /// Advisory note kept alongside a counted cell, such as
    /// `duplicateSuspected`. Never changes the totals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl DailyCell {
    fn counted(&self) -> bool {
        self.suppressed.is_none()
    }
}

/// `daily.json`: every cell, plus the hashes it was built from.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Daily {
    pub schema: u32,
    pub generated_at: String,
    /// `machineId/agent/utcDate` → the shard content hash it was rolled up from.
    #[serde(default)]
    pub based_on: BTreeMap<String, String>,
    /// UTC date → the cells recorded on it.
    #[serde(default)]
    pub days: BTreeMap<String, Vec<DailyCell>>,
}

impl Default for Daily {
    fn default() -> Self {
        Self {
            schema: ROLLUP_SCHEMA,
            generated_at: String::new(),
            based_on: BTreeMap::new(),
            days: BTreeMap::new(),
        }
    }
}

impl Daily {
    /// Whether this shard's contents are already accounted for.
    pub fn is_current(&self, reference: &ShardRef, content_hash: &str) -> bool {
        self.based_on.get(&reference.key()).map(String::as_str) == Some(content_hash)
    }

    /// Replaces everything this shard contributed. Applying the same shard twice
    /// is a no-op, which is what makes a retried sync safe.
    pub fn apply(&mut self, shard: &Shard) {
        let reference = ShardRef::of(shard);
        self.forget(&reference);
        let cells: Vec<DailyCell> = shard
            .cells
            .iter()
            .map(|cell| DailyCell {
                machine_id: shard.machine_id.clone(),
                agent: shard.agent.clone(),
                bucket: cell.bucket,
                model: cell.model.clone(),
                input_tokens: cell.input_tokens,
                output_tokens: cell.output_tokens,
                cache_write_tokens: cell.cache_write_tokens,
                cache_read_tokens: cell.cache_read_tokens,
                cost: cell.cost,
                messages: cell.messages,
                suppressed: None,
                notes: Vec::new(),
            })
            .collect();
        if !cells.is_empty() {
            let day = self.days.entry(reference.utc_date.clone()).or_default();
            day.extend(cells);
            sort_cells(day);
        }
        if let Some(hash) = shard.content_hash.clone() {
            self.based_on.insert(reference.key(), hash);
        }
    }

    /// Drops a shard's contribution, including its `based_on` entry, so a later
    /// sync rolls it up again from scratch.
    pub fn forget(&mut self, reference: &ShardRef) {
        self.based_on.remove(&reference.key());
        let Some(day) = self.days.get_mut(&reference.utc_date) else {
            return;
        };
        day.retain(|cell| cell.machine_id != reference.machine_id || cell.agent != reference.agent);
        if day.is_empty() {
            self.days.remove(&reference.utc_date);
        }
    }

    /// Drops every day a machine contributed — `sync forget <machine>`.
    pub fn forget_machine(&mut self, machine_id: &str) {
        self.based_on
            .retain(|key, _| !key.starts_with(&format!("{machine_id}/")));
        self.days.retain(|_, day| {
            day.retain(|cell| cell.machine_id != machine_id);
            !day.is_empty()
        });
    }

    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for cell in self.days.values().flatten().filter(|cell| cell.counted()) {
            totals.add(cell);
        }
        totals
    }
}

fn sort_cells(cells: &mut [DailyCell]) {
    cells.sort_by(|left, right| {
        (left.bucket, &left.machine_id, &left.agent, &left.model).cmp(&(
            right.bucket,
            &right.machine_id,
            &right.agent,
            &right.model,
        ))
    });
}

/// One row of `weekly.json` or `monthly.json`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Period {
    /// `2026-W38` for a week, `2026-09` for a month.
    pub period: String,
    pub totals: Totals,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_agent: BTreeMap<String, Totals>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_machine: BTreeMap<String, Totals>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_model: BTreeMap<String, Totals>,
}

/// `weekly.json` / `monthly.json`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Periodic {
    pub schema: u32,
    pub generated_at: String,
    pub periods: Vec<Period>,
}

/// `models.json`: the table the provider comparison is computed from.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Models {
    pub schema: u32,
    pub generated_at: String,
    /// Model name → totals across every machine, agent and day.
    pub models: BTreeMap<String, Totals>,
}

/// The three derived files, always produced together so they cannot disagree.
#[derive(Clone, Debug, PartialEq)]
pub struct Derived {
    pub weekly: Periodic,
    pub monthly: Periodic,
    pub models: Models,
}

pub fn derive(daily: &Daily) -> Derived {
    let mut weekly: BTreeMap<String, Period> = BTreeMap::new();
    let mut monthly: BTreeMap<String, Period> = BTreeMap::new();
    let mut models: BTreeMap<String, Totals> = BTreeMap::new();

    for (date, cells) in &daily.days {
        let week = iso_week(date);
        let month = date.get(..7).unwrap_or(date).to_string();
        for cell in cells.iter().filter(|cell| cell.counted()) {
            if let Some(week) = week.clone() {
                add_to_period(weekly.entry(week.clone()).or_default(), &week, cell);
            }
            add_to_period(monthly.entry(month.clone()).or_default(), &month, cell);
            models.entry(cell.model.clone()).or_default().add(cell);
        }
    }

    Derived {
        weekly: Periodic {
            schema: ROLLUP_SCHEMA,
            generated_at: daily.generated_at.clone(),
            periods: weekly.into_values().collect(),
        },
        monthly: Periodic {
            schema: ROLLUP_SCHEMA,
            generated_at: daily.generated_at.clone(),
            periods: monthly.into_values().collect(),
        },
        models: Models {
            schema: ROLLUP_SCHEMA,
            generated_at: daily.generated_at.clone(),
            models,
        },
    }
}

fn add_to_period(period: &mut Period, name: &str, cell: &DailyCell) {
    if period.period.is_empty() {
        period.period = name.to_string();
    }
    period.totals.add(cell);
    period
        .by_agent
        .entry(cell.agent.clone())
        .or_default()
        .add(cell);
    period
        .by_machine
        .entry(cell.machine_id.clone())
        .or_default()
        .add(cell);
    period
        .by_model
        .entry(cell.model.clone())
        .or_default()
        .add(cell);
}

/// The ISO-8601 week a UTC date falls in, as `YYYY-Www`.
///
/// ISO weeks rather than "weeks since some Sunday" because the year boundary is
/// where a hand-rolled week number goes wrong: 2027-01-01 belongs to `2026-W53`,
/// and a dashboard that puts it in `2027-W01` shows two short weeks instead of
/// one full one.
fn iso_week(date: &str) -> Option<String> {
    let civil: jiff::civil::Date = date.parse().ok()?;
    let week = civil.iso_week_date();
    Some(format!("{:04}-W{:02}", week.year(), week.week()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::{Cell, Dedupe, SHARD_SCHEMA};

    fn shard(machine: &str, agent: &str, date: &str, cells: Vec<Cell>) -> Shard {
        let mut shard = Shard {
            schema: SHARD_SCHEMA,
            agent: agent.to_string(),
            machine_id: machine.to_string(),
            user_id: "u-1234".to_string(),
            utc_date: date.to_string(),
            generated_at: "2026-09-17T18:12:03Z".to_string(),
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

    fn cell(bucket: u16, model: &str, input: u64, cost: f64) -> Cell {
        Cell {
            bucket,
            model: model.to_string(),
            input_tokens: input,
            output_tokens: 10,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            cost,
            messages: 1,
            keys: Vec::new(),
        }
    }

    fn recompute(shards: &[Shard]) -> Daily {
        let mut daily = Daily::default();
        for shard in shards {
            daily.apply(shard);
        }
        daily
    }

    #[test]
    fn two_machines_on_the_same_day_are_summed_not_overwritten() {
        let daily = recompute(&[
            shard(
                "aaaa",
                "claude",
                "2026-09-17",
                vec![cell(4, "sonnet", 100, 1.0)],
            ),
            shard(
                "bbbb",
                "claude",
                "2026-09-17",
                vec![cell(4, "sonnet", 50, 0.5)],
            ),
        ]);

        assert_eq!(daily.days["2026-09-17"].len(), 2);
        assert_eq!(daily.totals().input_tokens, 150);
        assert!((daily.totals().cost - 1.5).abs() < 1e-9);
    }

    /// The property the whole incremental design rests on: rolling a changed
    /// shard into an existing daily must equal rebuilding from every shard.
    #[test]
    fn an_incremental_update_equals_a_full_recompute() {
        let first = shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        );
        let other = shard(
            "bbbb",
            "codex",
            "2026-09-18",
            vec![cell(9, "gpt-5", 7, 0.1)],
        );
        let mut incremental = recompute(&[first.clone(), other.clone()]);

        // The same day re-folded after more local activity.
        let grown = shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0), cell(8, "opus", 40, 2.0)],
        );
        incremental.apply(&grown);

        assert_eq!(incremental, recompute(&[grown, other]));
    }

    #[test]
    fn re_applying_an_unchanged_shard_changes_nothing() {
        let shard = shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        );
        let mut daily = recompute(std::slice::from_ref(&shard));
        let before = daily.clone();

        daily.apply(&shard);

        assert_eq!(daily, before);
    }

    #[test]
    fn a_shrunken_shard_drops_the_cells_it_no_longer_has() {
        let big = shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0), cell(8, "opus", 40, 2.0)],
        );
        let mut daily = recompute(&[big]);

        daily.apply(&shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        ));

        assert_eq!(daily.days["2026-09-17"].len(), 1);
        assert_eq!(daily.totals().input_tokens, 100);
    }

    #[test]
    fn a_rolled_up_shard_is_recognised_by_its_hash() {
        let shard = shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        );
        let daily = recompute(std::slice::from_ref(&shard));
        let reference = ShardRef::of(&shard);
        let hash = shard.content_hash.clone().expect("finished");

        assert!(daily.is_current(&reference, &hash));
        assert!(!daily.is_current(&reference, "sha256:something-else"));
    }

    #[test]
    fn forgetting_a_machine_removes_its_days_and_its_hashes() {
        let mut daily = recompute(&[
            shard(
                "aaaa",
                "claude",
                "2026-09-17",
                vec![cell(4, "sonnet", 100, 1.0)],
            ),
            shard(
                "bbbb",
                "claude",
                "2026-09-17",
                vec![cell(4, "sonnet", 50, 0.5)],
            ),
            shard(
                "bbbb",
                "claude",
                "2026-09-18",
                vec![cell(4, "sonnet", 50, 0.5)],
            ),
        ]);

        daily.forget_machine("bbbb");

        assert_eq!(daily.days.keys().collect::<Vec<_>>(), vec!["2026-09-17"]);
        assert_eq!(daily.totals().input_tokens, 100);
        assert!(daily.based_on.keys().all(|key| key.starts_with("aaaa/")));
    }

    #[test]
    fn a_suppressed_cell_is_kept_for_display_but_left_out_of_the_totals() {
        let mut daily = recompute(&[shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        )]);
        daily.days.get_mut("2026-09-17").expect("day")[0].suppressed =
            Some("duplicateSuppressed".to_string());

        assert_eq!(daily.totals(), Totals::default());
        assert_eq!(derive(&daily).models.models.len(), 0);
    }

    #[test]
    fn weekly_and_monthly_split_the_same_cells_by_calendar() {
        let daily = recompute(&[
            // Sunday, ISO week 39 of 2026, and the last day of September.
            shard(
                "aaaa",
                "claude",
                "2026-09-27",
                vec![cell(4, "sonnet", 100, 1.0)],
            ),
            // Thursday, ISO week 40, October.
            shard(
                "aaaa",
                "codex",
                "2026-10-01",
                vec![cell(4, "gpt-5", 20, 0.2)],
            ),
        ]);

        let derived = derive(&daily);

        assert_eq!(
            derived
                .weekly
                .periods
                .iter()
                .map(|period| period.period.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-W39", "2026-W40"]
        );
        assert_eq!(
            derived
                .monthly
                .periods
                .iter()
                .map(|period| period.period.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-09", "2026-10"]
        );
        assert_eq!(derived.monthly.periods[0].totals.input_tokens, 100);
        assert_eq!(
            derived.monthly.periods[1].by_agent["codex"].input_tokens,
            20
        );
    }

    /// 2027-01-01 is a Friday and belongs to ISO week 53 of 2026; a naive
    /// "week of the year" would file it under 2027 and show two stub weeks.
    #[test]
    fn a_new_year_that_belongs_to_the_old_iso_week_is_filed_with_it() {
        let daily = recompute(&[
            shard(
                "aaaa",
                "claude",
                "2026-12-31",
                vec![cell(4, "sonnet", 10, 0.1)],
            ),
            shard(
                "aaaa",
                "claude",
                "2027-01-01",
                vec![cell(4, "sonnet", 10, 0.1)],
            ),
        ]);

        let derived = derive(&daily);

        assert_eq!(derived.weekly.periods.len(), 1);
        assert_eq!(derived.weekly.periods[0].period, "2026-W53");
        assert_eq!(derived.weekly.periods[0].totals.input_tokens, 20);
    }

    #[test]
    fn models_sum_across_machines_agents_and_days() {
        let daily = recompute(&[
            shard(
                "aaaa",
                "claude",
                "2026-09-17",
                vec![cell(4, "sonnet", 100, 1.0)],
            ),
            shard(
                "bbbb",
                "claude",
                "2026-09-18",
                vec![cell(4, "sonnet", 5, 0.05)],
            ),
            shard(
                "bbbb",
                "codex",
                "2026-09-18",
                vec![cell(4, "gpt-5", 7, 0.07)],
            ),
        ]);

        let models = derive(&daily).models.models;

        assert_eq!(models["sonnet"].input_tokens, 105);
        assert_eq!(models["gpt-5"].input_tokens, 7);
    }

    #[test]
    fn a_rollup_survives_a_round_trip_through_json() {
        let daily = recompute(&[shard(
            "aaaa",
            "claude",
            "2026-09-17",
            vec![cell(4, "sonnet", 100, 1.0)],
        )]);

        let bytes = serde_json::to_vec(&daily).expect("serialize");
        let parsed: Daily = serde_json::from_slice(&bytes).expect("parse");

        assert_eq!(parsed, daily);
    }
}
