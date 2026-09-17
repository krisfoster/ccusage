//! Turning the entries ccusage already parses out of agent logs into shards.
//!
//! The fold is deliberately provider-agnostic: it takes a flat list of
//! [`FoldEntry`] rather than any adapter's type, so the rules that decide which
//! 15-minute window an entry lands in, which entries are the same entry seen
//! twice, and what a project is allowed to be called in the bucket are all
//! testable without reading a log file.
//!
//! Two invariants hold everywhere below. Token counts are authoritative and are
//! summed exactly as the adapters reported them; cost is a convenience snapshot
//! the caller has already computed under the configured cost mode. And no raw
//! project path ever reaches a shard unless the user turned redaction off.

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;

use crate::salt::{DEDUPE_ALGORITHM, Salt};
use crate::shard::{BUCKETS_PER_DAY, Cell, Dedupe, SHARD_SCHEMA, Session, Shard};

const MS_PER_BUCKET: i64 = 15 * 60 * 1_000;

/// One entry as ccusage already understands it, stripped to what the bucket is
/// allowed to hold.
#[derive(Clone, Debug, Default)]
pub struct FoldEntry {
    pub timestamp_ms: i64,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    pub session_id: String,
    /// The local path. Hashed before it reaches a shard unless redaction is off.
    pub project_path: String,
    /// The provider's message ID, where the log carries one.
    pub message_id: Option<String>,
    /// The provider's request ID, where the log carries one.
    pub request_id: Option<String>,
}

/// Everything about the fold that comes from configuration rather than the logs.
pub struct FoldContext<'a> {
    pub agent: &'a str,
    pub machine_id: &'a str,
    pub user_id: &'a str,
    pub ccusage_version: &'a str,
    pub cost_mode: &'a str,
    pub pricing_snapshot: &'a str,
    pub generated_at: &'a str,
    pub salt: &'a Salt,
    /// Off only when the user has explicitly accepted that their project paths
    /// are readable by anyone who can read the bucket.
    pub redact_projects: bool,
}

/// Folds entries into one shard per UTC date, sorted by date.
///
/// Entries with no usable timestamp are dropped rather than guessed at: putting
/// them in "today" would move a user's history every time they sync.
pub fn fold(entries: &[FoldEntry], context: &FoldContext<'_>) -> Vec<Shard> {
    let mut days: BTreeMap<String, DayBuilder> = BTreeMap::new();
    for entry in entries {
        let Some((date, bucket)) = utc_date_and_bucket(entry.timestamp_ms) else {
            continue;
        };
        days.entry(date).or_default().add(entry, bucket, context);
    }
    days.into_iter()
        .map(|(date, day)| day.into_shard(&date, context))
        .collect()
}

/// The UTC date and 15-minute window index an instant belongs to.
pub fn utc_date_and_bucket(timestamp_ms: i64) -> Option<(String, u16)> {
    let timestamp = Timestamp::from_millisecond(timestamp_ms).ok()?;
    let zoned = timestamp.to_zoned(jiff::tz::TimeZone::UTC);
    let date = zoned.date().to_string();
    let ms_into_day = timestamp_ms.rem_euclid(86_400_000);
    let bucket = u16::try_from(ms_into_day / MS_PER_BUCKET).ok()?;
    (bucket < BUCKETS_PER_DAY).then_some((date, bucket))
}

#[derive(Default)]
struct DayBuilder {
    cells: BTreeMap<(u16, String), Cell>,
    sessions: BTreeMap<String, Session>,
    /// Keys already folded into this day, so the same entry read from two log
    /// files — or a re-run over the same files — is counted once.
    seen: HashSet<u64>,
}

impl DayBuilder {
    fn add(&mut self, entry: &FoldEntry, bucket: u16, context: &FoldContext<'_>) {
        let key = dedupe_key(entry, context.salt);
        if !self.seen.insert(key.0) {
            return;
        }
        let cell = self
            .cells
            .entry((bucket, entry.model.clone()))
            .or_insert_with(|| Cell {
                bucket,
                model: entry.model.clone(),
                ..Cell::default()
            });
        cell.input_tokens += entry.input_tokens;
        cell.output_tokens += entry.output_tokens;
        cell.cache_write_tokens += entry.cache_write_tokens;
        cell.cache_read_tokens += entry.cache_read_tokens;
        cell.cost += entry.cost;
        cell.messages += 1;
        cell.keys.push(key);

        let project = project_label(&entry.project_path, context);
        let session = self
            .sessions
            .entry(entry.session_id.clone())
            .or_insert_with(|| Session {
                id: entry.session_id.clone(),
                project,
                first: entry.timestamp_ms,
                last: entry.timestamp_ms,
                cost: 0.0,
            });
        session.first = session.first.min(entry.timestamp_ms);
        session.last = session.last.max(entry.timestamp_ms);
        session.cost += entry.cost;
    }

    fn into_shard(self, date: &str, context: &FoldContext<'_>) -> Shard {
        let mut shard = Shard {
            schema: SHARD_SCHEMA,
            agent: context.agent.to_string(),
            machine_id: context.machine_id.to_string(),
            user_id: context.user_id.to_string(),
            utc_date: date.to_string(),
            generated_at: context.generated_at.to_string(),
            ccusage_version: context.ccusage_version.to_string(),
            cost_mode: context.cost_mode.to_string(),
            pricing_snapshot: context.pricing_snapshot.to_string(),
            cells: self.cells.into_values().collect(),
            sessions: self.sessions.into_values().collect(),
            dedupe: Dedupe {
                algo: DEDUPE_ALGORITHM.to_string(),
                salt: context.salt.fingerprint(),
                count: 0,
            },
            content_hash: None,
        };
        shard.normalize();
        shard
    }
}

/// Provider IDs identify an entry across machines, which is the whole point of
/// the key. Logs that carry neither fall back to the fields that identify the
/// entry within a session, which still deduplicates re-reads on one machine and
/// is merely conservative across machines.
fn dedupe_key(entry: &FoldEntry, salt: &Salt) -> crate::shard::DedupeKey {
    match (entry.message_id.as_deref(), entry.request_id.as_deref()) {
        (None, None) => salt.dedupe_key(
            &format!(
                "{}|{}|{}|{}",
                entry.session_id, entry.timestamp_ms, entry.input_tokens, entry.output_tokens
            ),
            &entry.model,
        ),
        (message, request) => salt.dedupe_key(message.unwrap_or(""), request.unwrap_or("")),
    }
}

fn project_label(project_path: &str, context: &FoldContext<'_>) -> Option<String> {
    if project_path.is_empty() {
        return None;
    }
    Some(if context.redact_projects {
        context.salt.hash_project(project_path)
    } else {
        project_path.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT_HEX: &str = "00112233445566778899aabbccddeeff";

    fn salt() -> Salt {
        Salt::parse(SALT_HEX).expect("salt")
    }

    fn context<'a>(salt: &'a Salt, redact_projects: bool) -> FoldContext<'a> {
        FoldContext {
            agent: "claude",
            machine_id: "machine-1",
            user_id: "user-1",
            ccusage_version: "20.0.21",
            cost_mode: "auto",
            pricing_snapshot: "litellm@2026-09-16",
            generated_at: "2026-09-17T18:12:03Z",
            salt,
            redact_projects,
        }
    }

    /// 2026-09-17T00:07:30Z plus the offset, so bucket arithmetic is readable.
    fn at(offset_ms: i64) -> i64 {
        1_789_603_650_000 + offset_ms
    }

    fn entry(timestamp_ms: i64, model: &str, id: &str) -> FoldEntry {
        FoldEntry {
            timestamp_ms,
            model: model.to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cost: 0.25,
            session_id: "session-a".to_string(),
            project_path: "/home/me/dev/secret-project".to_string(),
            message_id: Some(id.to_string()),
            ..FoldEntry::default()
        }
    }

    #[test]
    fn entries_in_the_same_window_and_model_share_one_cell() {
        let salt = salt();
        let shards = fold(
            &[
                entry(at(0), "claude-sonnet-4-5", "m1"),
                entry(at(60_000), "claude-sonnet-4-5", "m2"),
            ],
            &context(&salt, true),
        );

        let [shard] = shards.as_slice() else {
            panic!("one date, one shard");
        };
        assert_eq!(shard.cells.len(), 1);
        assert_eq!(shard.cells[0].input_tokens, 20);
        assert_eq!(shard.cells[0].messages, 2);
        assert_eq!(shard.cells[0].keys.len(), 2);
    }

    #[test]
    fn different_models_in_one_window_stay_separate() {
        let salt = salt();
        let shards = fold(
            &[
                entry(at(0), "claude-sonnet-4-5", "m1"),
                entry(at(0), "claude-opus-4-1", "m2"),
            ],
            &context(&salt, true),
        );

        assert_eq!(shards[0].cells.len(), 2);
    }

    #[test]
    fn a_later_window_is_a_different_cell() {
        let salt = salt();
        let shards = fold(
            &[
                entry(at(0), "claude-sonnet-4-5", "m1"),
                entry(at(MS_PER_BUCKET), "claude-sonnet-4-5", "m2"),
            ],
            &context(&salt, true),
        );

        let buckets: Vec<u16> = shards[0].cells.iter().map(|cell| cell.bucket).collect();
        assert_eq!(buckets, vec![0, 1]);
    }

    /// The same JSONL line read from two files, or a second sync over the same
    /// logs, must not double the user's spend.
    #[test]
    fn the_same_entry_twice_is_counted_once() {
        let salt = salt();
        let repeated = entry(at(0), "claude-sonnet-4-5", "m1");

        let shards = fold(&[repeated.clone(), repeated], &context(&salt, true));

        assert_eq!(shards[0].cells[0].messages, 1);
        assert_eq!(shards[0].cells[0].input_tokens, 10);
    }

    /// Without provider IDs there is still enough to recognize a re-read.
    #[test]
    fn entries_without_provider_ids_still_deduplicate_within_a_machine() {
        let salt = salt();
        let mut anonymous = entry(at(0), "claude-sonnet-4-5", "unused");
        anonymous.message_id = None;
        anonymous.request_id = None;

        let shards = fold(&[anonymous.clone(), anonymous], &context(&salt, true));

        assert_eq!(shards[0].cells[0].messages, 1);
    }

    #[test]
    fn a_day_boundary_splits_the_entries_into_two_shards() {
        let salt = salt();
        let midnight = 1_789_603_200_000;
        let shards = fold(
            &[
                entry(midnight - 1, "claude-sonnet-4-5", "m1"),
                entry(midnight, "claude-sonnet-4-5", "m2"),
            ],
            &context(&salt, true),
        );

        let dates: Vec<&str> = shards.iter().map(|shard| shard.utc_date.as_str()).collect();
        assert_eq!(dates, vec!["2026-09-16", "2026-09-17"]);
        assert_eq!(shards[0].cells[0].bucket, BUCKETS_PER_DAY - 1);
        assert_eq!(shards[1].cells[0].bucket, 0);
    }

    #[test]
    fn a_project_path_never_reaches_a_shard_while_redaction_is_on() {
        let salt = salt();
        let shards = fold(
            &[entry(at(0), "claude-sonnet-4-5", "m1")],
            &context(&salt, true),
        );

        let project = shards[0].sessions[0].project.clone().expect("project");
        assert!(!project.contains("secret-project"), "{project}");
        assert_eq!(project, salt.hash_project("/home/me/dev/secret-project"));
    }

    #[test]
    fn turning_redaction_off_keeps_the_path_the_user_asked_to_see() {
        let salt = salt();
        let shards = fold(
            &[entry(at(0), "claude-sonnet-4-5", "m1")],
            &context(&salt, false),
        );

        assert_eq!(
            shards[0].sessions[0].project.as_deref(),
            Some("/home/me/dev/secret-project")
        );
    }

    #[test]
    fn a_session_spans_its_first_and_last_entry_of_the_day() {
        let salt = salt();
        let shards = fold(
            &[
                entry(at(60_000), "claude-sonnet-4-5", "m1"),
                entry(at(0), "claude-sonnet-4-5", "m2"),
            ],
            &context(&salt, true),
        );

        let session = &shards[0].sessions[0];
        assert_eq!(session.first, at(0));
        assert_eq!(session.last, at(60_000));
        assert_eq!(session.cost, 0.5);
    }

    #[test]
    fn a_folded_shard_carries_a_content_hash_once_finished() {
        let salt = salt();
        let mut shards = fold(
            &[entry(at(0), "claude-sonnet-4-5", "m1")],
            &context(&salt, true),
        );

        shards[0].finish().expect("finish");

        assert!(shards[0].content_hash_matches());
        assert_eq!(shards[0].dedupe.count, 1);
        assert_eq!(shards[0].dedupe.algo, DEDUPE_ALGORITHM);
    }

    /// Two machines with the same salt must produce intersecting keys, or the
    /// merge cannot tell shared activity from new activity.
    #[test]
    fn two_machines_with_one_salt_agree_on_the_key_for_an_entry() {
        let salt = salt();
        let mut first = context(&salt, true);
        first.machine_id = "machine-1";
        let mut second = context(&salt, true);
        second.machine_id = "machine-2";
        let shared = entry(at(0), "claude-sonnet-4-5", "m1");

        let left = fold(&[shared.clone()], &first);
        let right = fold(&[shared], &second);

        assert_eq!(left[0].cells[0].keys, right[0].cells[0].keys);
    }

    #[test]
    fn an_unrepresentable_timestamp_is_dropped_rather_than_dated_today() {
        let salt = salt();
        let mut broken = entry(at(0), "claude-sonnet-4-5", "m1");
        broken.timestamp_ms = i64::MAX;

        assert!(fold(&[broken], &context(&salt, true)).is_empty());
    }
}
