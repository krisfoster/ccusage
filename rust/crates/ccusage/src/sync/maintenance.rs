//! `ccusage sync repair`, `forget` and `merge-machine`: the operations that
//! change what the bucket claims rather than what this machine has logged.
//!
//! All three work from the shards outward. Shards are the only objects a
//! machine writes as a record of usage; manifest, indexes and rollups are
//! statements about them and can always be rebuilt by listing. That is what
//! makes repair possible at all, and it is why forget deletes shards first and
//! rewrites the derived objects after.

use ccusage_config::ConfigContext;
use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition, RollupKind};
use ccusage_sync::shard::{ParsedShard, Shard};
use std::collections::{BTreeMap, BTreeSet};

use super::machine::{self, IndexEntry, MachineIndex};
use super::run::parse_date;
use super::{bootstrap, lock, now_ms, rollups};
use crate::{
    cli::{SyncForgetArgs, SyncMergeMachineArgs, SyncRepairArgs},
    cli_error,
};

pub(crate) type Result<T> = std::result::Result<T, String>;

const DAY_MS: i64 = 86_400_000;

/// One shard as the bucket holds it, found by listing rather than by index.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct FoundShard {
    pub machine_id: String,
    pub agent: String,
    pub utc_date: String,
}

impl FoundShard {
    fn day_key(&self) -> String {
        MachineIndex::entry_key(&self.agent, &self.utc_date)
    }
}

/// What a repair pass found and rewrote.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepairSummary {
    pub machines: usize,
    pub shards: usize,
    /// Index entries that were missing, stale, or pointed at a shard the
    /// bucket does not hold.
    pub corrected: usize,
    /// Machines the manifest roster had lost.
    pub re_registered: usize,
    pub dry_run: bool,
}

impl RepairSummary {
    pub fn to_text(&self) -> String {
        let verb = if self.dry_run {
            "Would repair"
        } else {
            "Repaired"
        };
        let mut text = format!(
            "{verb} {} machine(s) holding {} shard(s).",
            self.machines, self.shards
        );
        if self.corrected > 0 {
            text.push_str(&format!(" {} index entr(ies) corrected.", self.corrected));
        }
        if self.re_registered > 0 {
            text.push_str(&format!(
                " {} machine(s) were missing from the manifest and are registered again.",
                self.re_registered
            ));
        }
        if self.corrected == 0 && self.re_registered == 0 {
            text.push_str(" Nothing was inconsistent.");
        }
        text
    }
}

/// What forgetting a machine removed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ForgetSummary {
    pub machine_id: String,
    pub objects: usize,
}

impl ForgetSummary {
    pub fn to_text(&self) -> String {
        format!(
            "Deleted {} object(s) for machine {} and rebuilt the rollups without it.",
            self.objects, self.machine_id
        )
    }
}

/// What a machine merge moved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MergeSummary {
    pub from: String,
    pub into: String,
    pub shards: usize,
}

impl MergeSummary {
    pub fn to_text(&self) -> String {
        format!(
            "Moved {} shard(s) from machine {} to machine {}.",
            self.shards, self.from, self.into
        )
    }
}

/// Every shard the bucket holds for this user, by listing.
///
/// Listing rather than reading the manifest is the point: a repair exists for
/// the case where the manifest or an index is the thing that is wrong.
pub(crate) fn discover(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
) -> Result<Vec<FoundShard>> {
    let prefix = keys
        .machines_prefix(user_id)
        .map_err(|error| error.to_string())?;
    let mut found: Vec<FoundShard> = store
        .list(&prefix)
        .map_err(|error| error.to_string())?
        .iter()
        .filter_map(|meta| parse_shard_path(&prefix, &meta.key))
        .collect();
    found.sort();
    Ok(found)
}

/// `<machines prefix>/<machine>/shards/<agent>/<YYYY>/<MM>/<DD>.json`, and
/// nothing else: `machine.json` and `index.json` sit under the same prefix.
fn parse_shard_path(prefix: &str, path: &str) -> Option<FoundShard> {
    let relative = path.strip_prefix(prefix)?;
    let parts: Vec<&str> = relative.split('/').collect();
    let [machine_id, "shards", agent, year, month, day] = parts.as_slice() else {
        return None;
    };
    let day = day.strip_suffix(".json")?;
    let utc_date = format!("{year}-{month}-{day}");
    parse_date(&utc_date).ok()?;
    Some(FoundShard {
        machine_id: (*machine_id).to_string(),
        agent: (*agent).to_string(),
        utc_date,
    })
}

/// Rebuilds the manifest roster, the machine indexes and the rollups from the
/// shards the bucket actually holds.
///
/// Late-edit history is carried over from the index being replaced where the
/// hash still matches. It is a record of something that happened, and repair
/// is not supposed to be a way to erase it; where the hash disagrees the index
/// was wrong about that shard anyway, so its history goes with it.
pub(crate) fn repair(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    now: &str,
    now_ms: i64,
    dry_run: bool,
) -> Result<RepairSummary> {
    let found = discover(store, keys, user_id)?;
    let mut by_machine: BTreeMap<String, Vec<FoundShard>> = BTreeMap::new();
    for shard in found {
        by_machine
            .entry(shard.machine_id.clone())
            .or_default()
            .push(shard);
    }
    let registered: BTreeSet<String> = bootstrap::manifest_machines(store, keys)?
        .into_iter()
        .collect();

    let mut summary = RepairSummary {
        machines: by_machine.len(),
        dry_run,
        ..RepairSummary::default()
    };
    for (machine_id, shards) in &by_machine {
        summary.shards += shards.len();
        let previous = machine::load_index(store, keys, user_id, machine_id)?;
        let rebuilt = rebuild_index(store, keys, user_id, shards, &previous, now, now_ms)?;
        summary.corrected += difference(&previous, &rebuilt);
        if !registered.contains(machine_id) {
            summary.re_registered += 1;
        }
        if dry_run {
            continue;
        }
        write_index(store, keys, user_id, machine_id, rebuilt)?;
        bootstrap::register_machine(store, keys, user_id, machine_id)?;
    }

    if !dry_run {
        // The rollups are rebuilt rather than reconciled: whatever made the
        // repair necessary may have reached them too, and they are cheap to
        // recompute from indexes that are now known to be right.
        discard_rollups(store, keys)?;
        rollups::refresh(store, keys, user_id, now)?;
    }
    Ok(summary)
}

/// Deletes every object a machine owns and rebuilds the rollups without it.
pub(crate) fn forget(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    now: &str,
) -> Result<ForgetSummary> {
    let prefix = keys
        .machine_prefix(user_id, machine_id)
        .map_err(|error| error.to_string())?;
    let objects = store.list(&prefix).map_err(|error| error.to_string())?;
    if objects.is_empty()
        && !bootstrap::manifest_machines(store, keys)?
            .iter()
            .any(|id| id == machine_id)
    {
        return Err(format!(
            "machine {machine_id} has nothing in this bucket. Run 'ccusage sync status' to see the machines it knows."
        ));
    }
    // The roster goes first: a machine still listed but half-deleted would
    // have the next rollup report its shards as missing.
    bootstrap::unregister_machine(store, keys, machine_id)?;
    for meta in &objects {
        let key = keys.listed(&meta.key).map_err(|error| error.to_string())?;
        store
            .delete(&key, &Precondition::None)
            .map_err(|error| error.to_string())?;
    }
    rollups::refresh(store, keys, user_id, now)?;
    Ok(ForgetSummary {
        machine_id: machine_id.to_string(),
        objects: objects.len(),
    })
}

/// Moves one machine's shards under another machine id, for the reinstall case
/// where the same person's usage has been split across two identities.
///
/// Refused when both machines hold the same day, because merging those would
/// have to decide which one is right: they were written from different logs
/// and only one of them can survive under a single key. Forgetting one of the
/// two is the explicit way to say which.
pub(crate) fn merge_machine(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    from: &str,
    into: &str,
    now: &str,
) -> Result<MergeSummary> {
    if from == into {
        return Err("a machine cannot be merged into itself.".to_string());
    }
    let found = discover(store, keys, user_id)?;
    let moving: Vec<&FoundShard> = found
        .iter()
        .filter(|shard| shard.machine_id == from)
        .collect();
    if moving.is_empty() {
        return Err(format!("machine {from} has no shards in this bucket."));
    }
    let target: BTreeSet<String> = found
        .iter()
        .filter(|shard| shard.machine_id == into)
        .map(FoundShard::day_key)
        .collect();
    let clashes: Vec<String> = moving
        .iter()
        .map(|shard| shard.day_key())
        .filter(|day| target.contains(day))
        .collect();
    if !clashes.is_empty() {
        return Err(format!(
            "machines {from} and {into} both hold {}; merging would drop one of them. Forget the machine whose data is wrong first: {}.",
            clashes.len(),
            clashes.join(", ")
        ));
    }

    let mut entries = BTreeMap::new();
    for found in &moving {
        let source = shard_key(
            keys,
            user_id,
            &found.machine_id,
            &found.agent,
            &found.utc_date,
        )?;
        let Some(mut shard) = read_shard(store, &source)? else {
            continue;
        };
        // The machine id is inside the shard and inside its content hash, so
        // the body is rewritten rather than copied: a shard that still names
        // the old machine would be re-uploaded by the next run as a change.
        shard.machine_id = into.to_string();
        shard.content_hash = None;
        let hash = shard
            .finish()
            .map_err(|error| error.to_string())?
            .to_string();
        let destination = shard_key(keys, user_id, into, &found.agent, &found.utc_date)?;
        let body = shard.to_json().map_err(|error| error.to_string())?;
        store
            .put(&destination, &body, "application/json", &Precondition::None)
            .map_err(|error| error.to_string())?;
        entries.insert(
            found.day_key(),
            IndexEntry {
                content_hash: hash,
                finalized: true,
                updated_at: now.to_string(),
                ..IndexEntry::default()
            },
        );
    }

    machine::update_index(store, keys, user_id, into, |index| {
        for (day, entry) in &entries {
            index.shards.insert(day.clone(), entry.clone());
        }
    })?;
    let summary = MergeSummary {
        from: from.to_string(),
        into: into.to_string(),
        shards: entries.len(),
    };
    // Reuses forget so the old machine leaves the roster, the bucket and the
    // rollups by exactly one code path.
    forget(store, keys, user_id, from, now)?;
    Ok(summary)
}

/// What retention removed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PruneSummary {
    pub deleted: Vec<String>,
    pub oldest_kept: Option<String>,
    pub dry_run: bool,
}

impl PruneSummary {
    pub fn to_text(&self) -> String {
        if self.deleted.is_empty() {
            return "Nothing to prune; no day is older than the retention window.".to_string();
        }
        let verb = if self.dry_run {
            "Would delete"
        } else {
            "Deleted"
        };
        format!(
            "{verb} {} day(s) beyond the retention window, oldest kept {}.",
            self.deleted.len(),
            self.oldest_kept.as_deref().unwrap_or("none")
        )
    }
}

/// Deletes shards for days older than `keep_days`, across every machine.
///
/// Index entries go with the objects, so the next rollup pass drops the pruned
/// days from the totals as well: the rollup is a statement about shards that
/// exist, and leaving a day in it that nothing backs would make the dashboard
/// charge for usage the bucket can no longer show. Retention therefore shortens
/// the history, which is why nothing prunes unless it is asked to.
pub(crate) fn prune(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    keep_days: u32,
    now_ms: i64,
    dry_run: bool,
) -> Result<PruneSummary> {
    let cutoff_ms = now_ms - i64::from(keep_days) * DAY_MS;
    let cutoff = crate::format_rfc3339_millis(ccusage_core::TimestampMs::from_millis(cutoff_ms))
        .split('T')
        .next()
        .unwrap_or_default()
        .to_string();
    let found = discover(store, keys, user_id)?;
    let mut summary = PruneSummary {
        dry_run,
        ..PruneSummary::default()
    };
    let mut by_machine: BTreeMap<&str, Vec<&FoundShard>> = BTreeMap::new();
    for shard in &found {
        if shard.utc_date < cutoff {
            by_machine
                .entry(shard.machine_id.as_str())
                .or_default()
                .push(shard);
            summary.deleted.push(format!(
                "{}/{}/{}",
                shard.machine_id, shard.agent, shard.utc_date
            ));
        } else {
            summary.oldest_kept = match summary.oldest_kept.take() {
                Some(kept) if kept <= shard.utc_date => Some(kept),
                _ => Some(shard.utc_date.clone()),
            };
        }
    }
    if dry_run || summary.deleted.is_empty() {
        return Ok(summary);
    }

    for (machine_id, shards) in by_machine {
        for shard in &shards {
            let key = shard_key(keys, user_id, machine_id, &shard.agent, &shard.utc_date)?;
            match store.delete(&key, &Precondition::None) {
                Ok(()) | Err(ObjectStoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        // The index entries go with the objects, or the next rollup pass reads
        // a shard that is not there and reports it as a failure every run.
        machine::update_index(store, keys, user_id, machine_id, |index| {
            for shard in &shards {
                index.shards.remove(&shard.day_key());
            }
        })?;
    }
    Ok(summary)
}

fn rebuild_index(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    shards: &[FoundShard],
    previous: &MachineIndex,
    now: &str,
    now_ms: i64,
) -> Result<MachineIndex> {
    let mut rebuilt = MachineIndex::default();
    for found in shards {
        let key = shard_key(
            keys,
            user_id,
            &found.machine_id,
            &found.agent,
            &found.utc_date,
        )?;
        let Some(shard) = read_shard(store, &key)? else {
            continue;
        };
        let Some(hash) = shard.content_hash.clone() else {
            continue;
        };
        let day = found.day_key();
        let carried = previous
            .shards
            .get(&day)
            .filter(|entry| entry.content_hash == hash);
        rebuilt.shards.insert(
            day,
            IndexEntry {
                content_hash: hash,
                finalized: carried.is_some_and(|entry| entry.finalized)
                    || ccusage_sync::is_finalized(&found.utc_date, now_ms),
                updated_at: carried
                    .map_or(now, |entry| entry.updated_at.as_str())
                    .to_string(),
                late_edits: carried.map_or(0, |entry| entry.late_edits),
                last_late_edit_at: carried.and_then(|entry| entry.last_late_edit_at.clone()),
            },
        );
    }
    Ok(rebuilt)
}

/// A shard this build cannot parse still exists and still belongs in the
/// index, so its hash is taken from the bytes the newer writer recorded.
fn read_shard(store: &dyn ObjectStore, key: &Key) -> Result<Option<Shard>> {
    let Some((body, _)) = store.get(key).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    match Shard::parse(&body).map_err(|error| error.to_string())? {
        ParsedShard::Known(shard) => Ok(Some(*shard)),
        ParsedShard::Newer { .. } => Ok(None),
    }
}

fn shard_key(
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    agent: &str,
    utc_date: &str,
) -> Result<Key> {
    let date = parse_date(utc_date)?;
    keys.shard(user_id, machine_id, agent, date)
        .map_err(|error| error.to_string())
}

fn write_index(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    rebuilt: MachineIndex,
) -> Result<()> {
    machine::update_index(store, keys, user_id, machine_id, |index| {
        index.shards = rebuilt.shards.clone();
    })
    .map(|_| ())
}

fn difference(previous: &MachineIndex, rebuilt: &MachineIndex) -> usize {
    let mut days: BTreeSet<&String> = previous.shards.keys().collect();
    days.extend(rebuilt.shards.keys());
    days.into_iter()
        .filter(|day| previous.shards.get(*day) != rebuilt.shards.get(*day))
        .count()
}

pub(crate) fn execute_repair(config: &ConfigContext, args: &SyncRepairArgs) -> crate::Result<()> {
    let session = super::connect(config)?;
    // Repair rewrites the same indexes and rollups a sync does, so it takes
    // the same machine-wide lock rather than racing one.
    let _lock = if args.dry_run {
        None
    } else {
        Some(lock::acquire(&lock::default_path(), now_ms()).map_err(cli_error)?)
    };
    let summary = repair(
        &session.store,
        &session.keys,
        &session.user_id,
        &iso_now(),
        now_ms(),
        args.dry_run,
    )
    .map_err(cli_error)?;
    println!("{}", summary.to_text());
    Ok(())
}

pub(crate) fn execute_forget(config: &ConfigContext, args: &SyncForgetArgs) -> crate::Result<()> {
    let session = super::connect(config)?;
    // Deleting the machine you are running on would leave this machine's
    // config pointing at data that no longer exists, and the next sync would
    // silently re-upload it.
    if args.machine == session.machine_id {
        return Err(cli_error(format!(
            "machine {} is this machine. Run 'ccusage sync forget' from another machine, or run setup again to change this machine's id.",
            args.machine
        )));
    }
    if !args.yes
        && !confirm(&format!(
            "Delete every uploaded day for machine {}? This cannot be undone.",
            args.machine
        ))
    {
        println!("Left the bucket unchanged.");
        return Ok(());
    }
    let _lock = lock::acquire(&lock::default_path(), now_ms()).map_err(cli_error)?;
    let summary = forget(
        &session.store,
        &session.keys,
        &session.user_id,
        &args.machine,
        &iso_now(),
    )
    .map_err(cli_error)?;
    println!("{}", summary.to_text());
    Ok(())
}

pub(crate) fn execute_merge(
    config: &ConfigContext,
    args: &SyncMergeMachineArgs,
) -> crate::Result<()> {
    let session = super::connect(config)?;
    if !args.yes
        && !confirm(&format!(
            "Move every uploaded day from machine {} to machine {}?",
            args.from, args.into
        ))
    {
        println!("Left the bucket unchanged.");
        return Ok(());
    }
    let _lock = lock::acquire(&lock::default_path(), now_ms()).map_err(cli_error)?;
    let summary = merge_machine(
        &session.store,
        &session.keys,
        &session.user_id,
        &args.from,
        &args.into,
        &iso_now(),
    )
    .map_err(cli_error)?;
    println!("{}", summary.to_text());
    Ok(())
}

/// Anything other than an explicit yes leaves the bucket alone, including a
/// pipe with nothing on it: these commands delete data.
fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    if std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn iso_now() -> String {
    crate::format_rfc3339_millis(ccusage_core::TimestampMs::from_millis(now_ms()))
}

/// Deleting rather than overwriting: the next refresh must start from nothing,
/// and a rollup left in place would keep whatever corruption it holds in its
/// `basedOn` map.
fn discard_rollups(store: &dyn ObjectStore, keys: &KeySpace) -> Result<()> {
    for kind in [RollupKind::Daily, RollupKind::Keys] {
        match store.delete(&keys.rollup(kind), &Precondition::None) {
            Ok(()) | Err(ObjectStoreError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ccusage_sync::rollup::Daily;
    use ccusage_sync::shard::{Cell, Dedupe, SHARD_SCHEMA};
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;

    const USER: &str = "user-1";
    const NOW: &str = "2026-09-17T18:12:03Z";
    const NOW_MS: i64 = 1_789_661_523_000;
    const AGENT: &str = "claude";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    fn shard(machine: &str, date: &str, input: u64) -> Shard {
        let mut shard = Shard {
            schema: SHARD_SCHEMA,
            agent: AGENT.to_string(),
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

    /// Writes a shard and the index entry and roster entry a healthy run would
    /// have left behind.
    fn publish(store: &MemoryStore, keys: &KeySpace, shard: &Shard) {
        put_shard(store, keys, shard);
        machine::update_index(store, keys, USER, &shard.machine_id, |index| {
            index.shards.insert(
                MachineIndex::entry_key(&shard.agent, &shard.utc_date),
                IndexEntry {
                    content_hash: shard.content_hash.clone().expect("finished"),
                    updated_at: NOW.to_string(),
                    ..IndexEntry::default()
                },
            );
        })
        .expect("index");
        bootstrap::ensure_manifest(store, keys, USER).expect("manifest");
        bootstrap::register_machine(store, keys, USER, &shard.machine_id).expect("register");
    }

    /// A shard written with no index entry and no roster entry: what a crash
    /// between the shard PUT and the index PUT leaves behind.
    fn put_shard(store: &MemoryStore, keys: &KeySpace, shard: &Shard) {
        let key =
            shard_key(keys, USER, &shard.machine_id, &shard.agent, &shard.utc_date).expect("key");
        store
            .put(
                &key,
                &shard.to_json().expect("serialize"),
                "application/json",
                &Precondition::None,
            )
            .expect("put");
    }

    fn daily(store: &MemoryStore, keys: &KeySpace) -> Daily {
        let (body, _) = store
            .get(&keys.rollup(RollupKind::Daily))
            .expect("get")
            .expect("daily exists");
        serde_json::from_slice(&body).expect("parse")
    }

    #[test]
    fn listing_finds_every_shard_and_ignores_the_machines_own_records() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("bbbb", "2026-09-16", 50));
        machine::update_machine(&store, &keys, USER, "aaaa", |_| {}).expect("machine.json");

        let found = discover(&store, &keys, USER).expect("discover");

        assert_eq!(
            found,
            vec![
                FoundShard {
                    machine_id: "aaaa".to_string(),
                    agent: AGENT.to_string(),
                    utc_date: "2026-09-17".to_string(),
                },
                FoundShard {
                    machine_id: "bbbb".to_string(),
                    agent: AGENT.to_string(),
                    utc_date: "2026-09-16".to_string(),
                },
            ]
        );
    }

    /// The case repair exists for: a shard reached the bucket but the index
    /// and roster writes never happened, so nothing counts it.
    #[test]
    fn repair_reindexes_a_shard_whose_index_write_never_landed() {
        let store = MemoryStore::new();
        let keys = keys();
        put_shard(&store, &keys, &shard("aaaa", "2026-09-17", 100));

        let summary = repair(&store, &keys, USER, NOW, NOW_MS, false).expect("repair");

        assert_eq!(summary.machines, 1);
        assert_eq!(summary.shards, 1);
        assert_eq!(summary.corrected, 1);
        assert_eq!(summary.re_registered, 1);
        assert_eq!(daily(&store, &keys).totals().input_tokens, 100);
    }

    #[test]
    fn a_dry_run_repair_reports_the_same_work_and_writes_nothing() {
        let store = MemoryStore::new();
        let keys = keys();
        put_shard(&store, &keys, &shard("aaaa", "2026-09-17", 100));

        let summary = repair(&store, &keys, USER, NOW, NOW_MS, true).expect("repair");

        assert_eq!(summary.corrected, 1);
        assert!(summary.to_text().starts_with("Would repair"));
        assert!(
            machine::load_index(&store, &keys, USER, "aaaa")
                .expect("index")
                .shards
                .is_empty()
        );
    }

    /// Deliberate corruption, then repair: the rollup must come back to the
    /// value a clean build produces, not to the corrupted one plus a delta.
    #[test]
    fn repair_rebuilds_rollups_that_were_corrupted() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");
        let healthy = daily(&store, &keys);
        store
            .put(
                &keys.rollup(RollupKind::Daily),
                br#"{"schema":1,"generatedAt":"2026-09-17T18:12:03Z","basedOn":{"aaaa/claude/2026-09-17":"wrong"},"days":{"2026-09-17":[{"i":4,"a":"claude","m":"claude-sonnet-4-5","it":999999}]}}"#,
                "application/json",
                &Precondition::None,
            )
            .expect("corrupt");

        repair(&store, &keys, USER, NOW, NOW_MS, false).expect("repair");

        assert_eq!(daily(&store, &keys).days, healthy.days);
        assert_eq!(daily(&store, &keys).based_on, healthy.based_on);
    }

    #[test]
    fn repair_leaves_a_healthy_bucket_alone() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");

        let summary = repair(&store, &keys, USER, NOW, NOW_MS, false).expect("repair");

        assert_eq!(summary.corrected, 0);
        assert_eq!(summary.re_registered, 0);
        assert!(summary.to_text().contains("Nothing was inconsistent"));
    }

    /// A late edit is a record of something that happened; repair is not a way
    /// to launder it away.
    #[test]
    fn repair_keeps_the_late_edit_history_of_an_unchanged_shard() {
        let store = MemoryStore::new();
        let keys = keys();
        let shard = shard("aaaa", "2026-09-17", 100);
        publish(&store, &keys, &shard);
        machine::update_index(&store, &keys, USER, "aaaa", |index| {
            let entry = index
                .shards
                .get_mut(&MachineIndex::entry_key(AGENT, "2026-09-17"))
                .expect("entry");
            entry.finalized = true;
            entry.late_edits = 2;
            entry.last_late_edit_at = Some(NOW.to_string());
        })
        .expect("seed");

        repair(&store, &keys, USER, NOW, NOW_MS, false).expect("repair");

        let entry = machine::load_index(&store, &keys, USER, "aaaa")
            .expect("index")
            .shards[&MachineIndex::entry_key(AGENT, "2026-09-17")]
            .clone();
        assert_eq!(entry.late_edits, 2);
        assert!(entry.finalized);
    }

    #[test]
    fn forgetting_a_machine_deletes_its_data_and_its_share_of_the_totals() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("bbbb", "2026-09-17", 25));
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");

        let summary = forget(&store, &keys, USER, "bbbb", NOW).expect("forget");

        assert_eq!(summary.objects, 2);
        assert_eq!(daily(&store, &keys).totals().input_tokens, 100);
        assert_eq!(
            bootstrap::manifest_machines(&store, &keys).expect("roster"),
            vec!["aaaa".to_string()]
        );
        assert!(
            store
                .list(&keys.machine_prefix(USER, "bbbb").expect("prefix"))
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn forgetting_a_machine_the_bucket_never_had_is_an_error() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));

        let error = forget(&store, &keys, USER, "cccc", NOW).expect_err("unknown machine");

        assert!(error.contains("nothing in this bucket"), "{error}");
    }

    /// The reinstall case: the same person, two machine ids, no day in common.
    #[test]
    fn merging_a_machine_moves_its_days_under_the_new_id() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("old1", "2026-09-16", 40));
        publish(&store, &keys, &shard("new1", "2026-09-17", 60));
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");

        let summary = merge_machine(&store, &keys, USER, "old1", "new1", NOW).expect("merge");

        assert_eq!(summary.shards, 1);
        assert_eq!(
            bootstrap::manifest_machines(&store, &keys).expect("roster"),
            vec!["new1".to_string()]
        );
        // The totals are unchanged: the usage moved, it did not vanish or double.
        assert_eq!(daily(&store, &keys).totals().input_tokens, 100);
        let moved = discover(&store, &keys, USER).expect("discover");
        assert!(moved.iter().all(|shard| shard.machine_id == "new1"));
    }

    /// The moved shard names its new machine, or the next run on that machine
    /// would see a hash mismatch and re-upload every day it inherited.
    #[test]
    fn a_merged_shard_is_rewritten_with_the_new_machine_id() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("old1", "2026-09-16", 40));
        publish(&store, &keys, &shard("new1", "2026-09-17", 60));

        merge_machine(&store, &keys, USER, "old1", "new1", NOW).expect("merge");

        let key = shard_key(&keys, USER, "new1", AGENT, "2026-09-16").expect("key");
        let moved = read_shard(&store, &key).expect("read").expect("moved");
        assert_eq!(moved.machine_id, "new1");
        let index = machine::load_index(&store, &keys, USER, "new1").expect("index");
        assert_eq!(
            index.shards[&MachineIndex::entry_key(AGENT, "2026-09-16")].content_hash,
            moved.content_hash.expect("hash")
        );
    }

    /// Two machines holding the same day were written from different logs, and
    /// one key can only hold one of them — so the user picks, not the merge.
    #[test]
    fn merging_machines_that_share_a_day_is_refused() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("old1", "2026-09-17", 40));
        publish(&store, &keys, &shard("new1", "2026-09-17", 60));

        let error =
            merge_machine(&store, &keys, USER, "old1", "new1", NOW).expect_err("overlapping days");

        assert!(error.contains("claude/2026-09-17"), "{error}");
        assert_eq!(discover(&store, &keys, USER).expect("discover").len(), 2);
    }

    #[test]
    fn pruning_deletes_days_beyond_the_window_and_keeps_the_rest() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));
        publish(&store, &keys, &shard("aaaa", "2026-08-01", 70));
        publish(&store, &keys, &shard("bbbb", "2026-06-01", 30));
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");

        let summary = prune(&store, &keys, USER, 30, NOW_MS, false).expect("prune");
        rollups::refresh(&store, &keys, USER, NOW).expect("rollup");

        assert_eq!(
            summary.deleted,
            vec![
                "aaaa/claude/2026-08-01".to_string(),
                "bbbb/claude/2026-06-01".to_string()
            ]
        );
        assert_eq!(summary.oldest_kept.as_deref(), Some("2026-09-17"));
        assert_eq!(daily(&store, &keys).totals().input_tokens, 100);
        // The machines themselves survive; only their old days are gone.
        assert_eq!(
            bootstrap::manifest_machines(&store, &keys).expect("roster"),
            vec!["aaaa".to_string(), "bbbb".to_string()]
        );
    }

    #[test]
    fn a_dry_run_prune_reports_the_same_days_and_deletes_nothing() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-08-01", 70));

        let summary = prune(&store, &keys, USER, 30, NOW_MS, true).expect("prune");

        assert_eq!(summary.deleted.len(), 1);
        assert!(summary.to_text().starts_with("Would delete"));
        assert_eq!(discover(&store, &keys, USER).expect("discover").len(), 1);
    }

    #[test]
    fn pruning_a_bucket_inside_the_window_changes_nothing() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 100));

        let summary = prune(&store, &keys, USER, 365, NOW_MS, false).expect("prune");

        assert!(summary.deleted.is_empty());
        assert!(summary.to_text().contains("Nothing to prune"));
        assert_eq!(discover(&store, &keys, USER).expect("discover").len(), 1);
    }

    #[test]
    fn merging_a_machine_into_itself_is_refused() {
        let store = MemoryStore::new();
        let keys = keys();
        publish(&store, &keys, &shard("aaaa", "2026-09-17", 40));

        let error =
            merge_machine(&store, &keys, USER, "aaaa", "aaaa", NOW).expect_err("same machine");

        assert!(error.contains("cannot be merged into itself"), "{error}");
    }
}
