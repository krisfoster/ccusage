//! `ccusage sync machines`: who has synced into this store, and who has gone quiet.
//!
//! A sandbox that is removed never says goodbye. Its usage stays in the totals, which is what is
//! wanted, but nothing marks it as gone, so the list of machines grows for good. The only evidence is
//! the time of its last sync, so a machine that has not synced for a while is called retired. The
//! store is never changed by this; it is a label for people and for the dashboard.

use ccusage_config::ConfigContext;
use ccusage_objectstore::{KeySpace, ObjectStore};
use serde_json::{Value, json};

use super::{bootstrap, machine, now_ms, status};
use crate::{Result, cli::SyncMachinesArgs, cli_error, gcs::parse_rfc3339_ms};

/// Long enough that a laptop left shut over a holiday is still active, short enough that a removed
/// sandbox stops looking alive within a fortnight.
pub(crate) const DEFAULT_RETIRED_AFTER_DAYS: u32 = 14;
const DAY_MS: i64 = 86_400_000;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MachineSummary {
    pub id: String,
    pub label: Option<String>,
    pub os: Option<String>,
    pub last_sync_at: Option<String>,
    /// Whole days since the last sync, or `None` for a machine that never recorded one.
    pub days_since_sync: Option<i64>,
    pub retired: bool,
    /// Distinct usage days the machine has uploaded, across agents.
    pub days: usize,
    pub agents: Vec<String>,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
}

/// Every machine on the roster, most recently synced first and retired ones last.
pub(crate) fn list(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    now: i64,
    retired_after_days: u32,
) -> std::result::Result<Vec<MachineSummary>, String> {
    let Some(user_id) = bootstrap::manifest_user_id(store, keys)? else {
        return Ok(Vec::new());
    };
    let mut machines = Vec::new();
    for id in bootstrap::manifest_machines(store, keys)? {
        let record = machine::load_record(store, keys, &user_id, &id)?;
        let index = machine::load_index(store, keys, &user_id, &id)?;

        let last_sync_ms = record.last_sync_at.as_deref().and_then(parse_rfc3339_ms);
        let days_since_sync = last_sync_ms.map(|last| (now - last).max(0) / DAY_MS);
        let retired = days_since_sync.is_some_and(|days| days >= i64::from(retired_after_days));

        let mut agents = Vec::new();
        let mut usage_days = std::collections::BTreeSet::new();
        for entry in index.shards.keys() {
            if let Some((agent, date)) = entry.split_once('/') {
                if !agents.iter().any(|known| known == agent) {
                    agents.push(agent.to_string());
                }
                usage_days.insert(date.to_string());
            }
        }
        agents.sort_unstable();
        machines.push(MachineSummary {
            id,
            label: record.label,
            os: record.os,
            last_sync_at: record.last_sync_at,
            days_since_sync,
            retired,
            days: usage_days.len(),
            agents,
            first_day: usage_days.first().cloned(),
            last_day: usage_days.last().cloned(),
        });
    }
    machines.sort_by(|left, right| {
        left.retired
            .cmp(&right.retired)
            .then_with(|| right.last_sync_at.cmp(&left.last_sync_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(machines)
}

pub(crate) fn to_json(
    machines: &[MachineSummary],
    retired_after_days: u32,
    generated_at: &str,
) -> Value {
    json!({
        "schema": 1,
        "generatedAt": generated_at,
        "retiredAfterDays": retired_after_days,
        "machines": machines.iter().map(|machine| json!({
            "id": machine.id,
            "label": machine.label,
            "os": machine.os,
            "lastSyncAt": machine.last_sync_at,
            "daysSinceSync": machine.days_since_sync,
            "retired": machine.retired,
            "days": machine.days,
            "agents": machine.agents,
            "firstDay": machine.first_day,
            "lastDay": machine.last_day,
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn to_text(machines: &[MachineSummary], retired_after_days: u32) -> String {
    if machines.is_empty() {
        return "No machine has synced into this store yet.".to_string();
    }
    let rows: Vec<[String; 6]> = machines
        .iter()
        .map(|machine| {
            let name = match &machine.label {
                Some(label) if label != &machine.id => format!("{label} ({})", short(&machine.id)),
                _ => short(&machine.id),
            };
            let state = match (machine.retired, machine.days_since_sync) {
                (true, _) => "retired",
                (false, Some(_)) => "active",
                (false, None) => "never synced",
            };
            let last = match machine.days_since_sync {
                Some(0) => "today".to_string(),
                Some(1) => "1 day ago".to_string(),
                Some(days) => format!("{days} days ago"),
                None => "-".to_string(),
            };
            let span = match (&machine.first_day, &machine.last_day) {
                (Some(first), Some(last)) => format!("{first} to {last}"),
                _ => "-".to_string(),
            };
            [
                name,
                state.to_string(),
                last,
                machine.days.to_string(),
                span,
                machine.agents.join(", "),
            ]
        })
        .collect();
    let headers = ["MACHINE", "STATUS", "LAST SYNC", "DAYS", "USAGE", "AGENTS"];
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain(std::iter::once(headers[column].len()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(column, cell)| format!("{cell:<width$}", width = widths[column]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut lines = vec![line(headers.to_vec())];
    lines.extend(
        rows.iter()
            .map(|row| line(row.iter().map(String::as_str).collect())),
    );
    lines.push(String::new());
    lines.push(format!(
        "A machine is retired when it has not synced for {retired_after_days} days. Its usage stays in the totals."
    ));
    lines.join("\n")
}

/// A UUID is too long for a table and the first block says enough to tell sandboxes apart.
fn short(id: &str) -> String {
    id.split('-')
        .next()
        .filter(|head| head.len() >= 6)
        .unwrap_or(id)
        .to_string()
}

pub(crate) fn execute(config: &ConfigContext, args: &SyncMachinesArgs, json: bool) -> Result<()> {
    let state = status::Status::from_config(config.sync());
    if !state.is_configured() {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    }
    let keys = KeySpace::new(&state.prefix).map_err(|error| cli_error(error.to_string()))?;
    let store = super::open_store(config, &state)?;
    let retired_after = args
        .retired_after_days
        .unwrap_or(DEFAULT_RETIRED_AFTER_DAYS);
    let machines = list(&store, &keys, now_ms(), retired_after).map_err(cli_error)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&to_json(
                &machines,
                retired_after,
                &super::run::iso_now()
            ))
            .map_err(|error| cli_error(error.to_string()))?
        );
    } else {
        println!("{}", to_text(&machines, retired_after));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;
    use crate::sync::machine::IndexEntry;

    const USER: &str = "user-1";
    const NOW: &str = "2026-10-10T12:00:00Z";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("key space")
    }

    fn now() -> i64 {
        parse_rfc3339_ms(NOW).expect("a fixed time")
    }

    /// Adds a machine to the roster, with whatever it last recorded about itself and its uploads.
    fn join(
        store: &MemoryStore,
        id: &str,
        label: Option<&str>,
        last_sync: Option<&str>,
        shards: &[&str],
    ) {
        let keys = keys();
        bootstrap::ensure_manifest(store, &keys, USER).expect("manifest");
        bootstrap::register_machine(store, &keys, USER, id).expect("register");
        machine::update_machine(store, &keys, USER, id, |record| {
            record.label = label.map(str::to_string);
            record.os = Some("linux".to_string());
            record.last_sync_at = last_sync.map(str::to_string);
        })
        .expect("record");
        machine::update_index(store, &keys, USER, id, |index| {
            for shard in shards {
                index.shards.insert(
                    (*shard).to_string(),
                    IndexEntry {
                        content_hash: "sha256:x".to_string(),
                        updated_at: NOW.to_string(),
                        ..IndexEntry::default()
                    },
                );
            }
        })
        .expect("index");
    }

    #[test]
    fn a_store_nobody_has_synced_into_lists_nothing() {
        let store = MemoryStore::new();

        let machines = list(&store, &keys(), now(), 14).expect("list");

        assert!(machines.is_empty());
        assert!(to_text(&machines, 14).contains("No machine has synced"));
    }

    #[test]
    fn a_machine_is_retired_after_the_threshold_and_not_before() {
        let store = MemoryStore::new();
        join(&store, "m-13", None, Some("2026-09-27T12:00:00Z"), &[]);
        join(&store, "m-14", None, Some("2026-09-26T12:00:00Z"), &[]);
        join(&store, "m-today", None, Some("2026-10-10T11:00:00Z"), &[]);

        let machines = list(&store, &keys(), now(), 14).expect("list");
        let retired = |id: &str| {
            machines
                .iter()
                .find(|m| m.id == id)
                .expect("machine")
                .retired
        };

        assert!(!retired("m-today"));
        assert!(!retired("m-13"), "13 days of silence is not yet 14");
        assert!(retired("m-14"));
    }

    #[test]
    fn active_machines_come_first_newest_sync_first_and_retired_ones_last() {
        let store = MemoryStore::new();
        join(&store, "old", None, Some("2026-08-01T00:00:00Z"), &[]);
        join(&store, "recent", None, Some("2026-10-10T10:00:00Z"), &[]);
        join(&store, "earlier", None, Some("2026-10-08T10:00:00Z"), &[]);

        let ids: Vec<_> = list(&store, &keys(), now(), 14)
            .expect("list")
            .into_iter()
            .map(|machine| machine.id)
            .collect();

        assert_eq!(ids, vec!["recent", "earlier", "old"]);
    }

    #[test]
    fn a_machine_that_never_recorded_a_sync_is_not_called_retired() {
        let store = MemoryStore::new();
        join(&store, "fresh", Some("fresh sandbox"), None, &[]);

        let machines = list(&store, &keys(), now(), 14).expect("list");

        assert!(!machines[0].retired);
        assert_eq!(machines[0].days_since_sync, None);
        assert!(to_text(&machines, 14).contains("never synced"));
    }

    #[test]
    fn usage_days_and_agents_come_from_what_the_machine_uploaded() {
        let store = MemoryStore::new();
        join(
            &store,
            "m1",
            Some("my sandbox"),
            Some(NOW),
            &["claude/2026-09-30", "claude/2026-10-02", "codex/2026-10-02"],
        );

        let machine = &list(&store, &keys(), now(), 14).expect("list")[0];

        assert_eq!(
            machine.days, 2,
            "a day counts once however many agents used it"
        );
        assert_eq!(machine.agents, vec!["claude", "codex"]);
        assert_eq!(machine.first_day.as_deref(), Some("2026-09-30"));
        assert_eq!(machine.last_day.as_deref(), Some("2026-10-02"));
    }

    #[test]
    fn the_table_names_a_sandbox_by_its_label_with_the_start_of_its_id() {
        let store = MemoryStore::new();
        join(
            &store,
            "687f69d9-8946-4e4d-a816-9cd3f36cf442",
            Some("my-sandbox"),
            Some(NOW),
            &["claude/2026-10-02"],
        );

        let text = to_text(&list(&store, &keys(), now(), 14).expect("list"), 14);

        assert!(text.contains("my-sandbox (687f69d9)"), "{text}");
        assert!(text.contains("active"), "{text}");
        assert!(text.contains("today"), "{text}");
    }

    #[test]
    fn the_json_shape_is_stable_for_scripts_and_the_dashboard() {
        let store = MemoryStore::new();
        join(
            &store,
            "m1",
            Some("label"),
            Some("2026-09-01T12:00:00Z"),
            &["claude/2026-08-30"],
        );

        let machines = list(&store, &keys(), now(), 14).expect("list");

        assert_eq!(
            to_json(&machines, 14, NOW),
            json!({
                "schema": 1,
                "generatedAt": NOW,
                "retiredAfterDays": 14,
                "machines": [{
                    "id": "m1",
                    "label": "label",
                    "os": "linux",
                    "lastSyncAt": "2026-09-01T12:00:00Z",
                    "daysSinceSync": 39,
                    "retired": true,
                    "days": 1,
                    "agents": ["claude"],
                    "firstDay": "2026-08-30",
                    "lastDay": "2026-08-30",
                }],
            })
        );
    }
}
