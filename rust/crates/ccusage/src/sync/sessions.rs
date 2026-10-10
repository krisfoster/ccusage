//! The sessions behind the totals, for the dashboard's drill-down.
//!
//! The rollups the dashboard reads are summed per machine, agent, model and 15-minute window, so they
//! cannot say which project or session spent what. The shards can: each records, per day, the sessions
//! that were active, with the project, the first and last activity, and the cost. This reads them all
//! and merges a session that crossed midnight back into one. There are no per-session token counts
//! anywhere, so cost is all it can say.

use std::collections::BTreeMap;

use ccusage_objectstore::{KeySpace, ObjectStore};
use ccusage_sync::{ParsedShard, Shard};
use serde_json::{Value, json};

use super::bootstrap;

/// A dashboard that has to draw this many rows is not going to be read row by row, and the newest are
/// the ones asked about.
pub(crate) const MAX_SESSIONS: usize = 5_000;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionSummary {
    pub machine: String,
    pub agent: String,
    pub id: String,
    /// The project path, or a salted hash of it unless redaction was turned off.
    pub project: Option<String>,
    pub first_ms: i64,
    pub last_ms: i64,
    pub cost: f64,
    pub days: usize,
}

pub(crate) struct Sessions {
    pub sessions: Vec<SessionSummary>,
    /// How many there were before the list was cut to [`MAX_SESSIONS`].
    pub total: usize,
}

/// Every session in the store, newest first.
pub(crate) fn collect(
    store: &dyn ObjectStore,
    keys: &KeySpace,
) -> std::result::Result<Sessions, String> {
    let Some(user_id) = bootstrap::manifest_user_id(store, keys)? else {
        return Ok(Sessions {
            sessions: Vec::new(),
            total: 0,
        });
    };
    let mut merged: BTreeMap<(String, String, String), SessionSummary> = BTreeMap::new();
    for machine in bootstrap::manifest_machines(store, keys)? {
        let prefix = keys
            .machine_prefix(&user_id, &machine)
            .map_err(|error| error.to_string())?;
        for object in store.list(&prefix).map_err(|error| error.to_string())? {
            if !object.key.contains("/shards/") {
                continue;
            }
            let key = keys
                .listed(&object.key)
                .map_err(|error| error.to_string())?;
            let Some((body, _)) = store.get(&key).map_err(|error| error.to_string())? else {
                continue;
            };
            // A shard from a newer ccusage, or one that does not parse, is skipped: the drill-down is
            // a convenience, and the totals do not depend on it.
            let Ok(ParsedShard::Known(shard)) = Shard::parse(&body) else {
                continue;
            };
            fold_in(&mut merged, &machine, &shard);
        }
    }
    let mut sessions: Vec<SessionSummary> = merged.into_values().collect();
    sessions.sort_by(|left, right| {
        right
            .last_ms
            .cmp(&left.last_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let total = sessions.len();
    sessions.truncate(MAX_SESSIONS);
    Ok(Sessions { sessions, total })
}

fn fold_in(
    merged: &mut BTreeMap<(String, String, String), SessionSummary>,
    machine: &str,
    shard: &Shard,
) {
    for session in &shard.sessions {
        let entry = merged
            .entry((machine.to_string(), shard.agent.clone(), session.id.clone()))
            .or_insert_with(|| SessionSummary {
                machine: machine.to_string(),
                agent: shard.agent.clone(),
                id: session.id.clone(),
                project: None,
                first_ms: session.first,
                last_ms: session.last,
                cost: 0.0,
                days: 0,
            });
        entry.first_ms = entry.first_ms.min(session.first);
        entry.last_ms = entry.last_ms.max(session.last);
        entry.cost += session.cost;
        entry.days += 1;
        if entry.project.is_none() {
            entry.project.clone_from(&session.project);
        }
    }
}

pub(crate) fn to_json(sessions: &Sessions, generated_at: &str) -> Value {
    json!({
        "schema": 1,
        "generatedAt": generated_at,
        "total": sessions.total,
        "truncated": sessions.total > sessions.sessions.len(),
        "sessions": sessions.sessions.iter().map(|session| json!({
            "machine": session.machine,
            "agent": session.agent,
            "id": session.id,
            "project": session.project,
            "first": session.first_ms,
            "last": session.last_ms,
            "cost": session.cost,
            "days": session.days,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use ccusage_objectstore::{Precondition, UtcDate};
    use ccusage_sync::{Cell, Dedupe, SHARD_SCHEMA, Session};
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;

    const USER: &str = "user-1";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("key space")
    }

    fn session(id: &str, project: Option<&str>, first: i64, last: i64, cost: f64) -> Session {
        Session {
            id: id.to_string(),
            project: project.map(str::to_string),
            first,
            last,
            cost,
        }
    }

    /// Writes one day's shard for a machine, and puts the machine on the roster.
    fn upload(
        store: &MemoryStore,
        machine: &str,
        agent: &str,
        date: (i16, u8, u8),
        sessions: Vec<Session>,
    ) {
        let keys = keys();
        bootstrap::ensure_manifest(store, &keys, USER).expect("manifest");
        bootstrap::register_machine(store, &keys, USER, machine).expect("register");
        let mut shard = Shard {
            schema: SHARD_SCHEMA,
            agent: agent.to_string(),
            machine_id: machine.to_string(),
            user_id: USER.to_string(),
            utc_date: format!("{:04}-{:02}-{:02}", date.0, date.1, date.2),
            generated_at: "2026-10-10T12:00:00Z".to_string(),
            ccusage_version: "20.0.21".to_string(),
            cost_mode: "auto".to_string(),
            pricing_snapshot: "embedded".to_string(),
            cells: vec![Cell {
                bucket: 1,
                model: "m".to_string(),
                input_tokens: 1,
                ..Cell::default()
            }],
            sessions,
            dedupe: Dedupe {
                algo: "sha256-64/v1".to_string(),
                salt: "salt:x".to_string(),
                count: 0,
            },
            content_hash: None,
        };
        shard.finish().expect("finish");
        let key = keys
            .shard(
                USER,
                machine,
                agent,
                UtcDate::new(date.0, date.1, date.2).expect("date"),
            )
            .expect("key");
        store
            .put(
                &key,
                &shard.to_json().expect("json"),
                "application/json",
                &Precondition::None,
            )
            .expect("put");
    }

    #[test]
    fn a_store_with_nothing_in_it_has_no_sessions() {
        let store = MemoryStore::new();

        let found = collect(&store, &keys()).expect("collect");

        assert!(found.sessions.is_empty());
        assert_eq!(found.total, 0);
    }

    #[test]
    fn a_session_that_crossed_midnight_is_one_session_with_the_whole_span_and_cost() {
        let store = MemoryStore::new();
        upload(
            &store,
            "m1",
            "claude",
            (2026, 10, 9),
            vec![session("s1", Some("/work/a"), 1_000, 2_000, 1.5)],
        );
        upload(
            &store,
            "m1",
            "claude",
            (2026, 10, 10),
            vec![session("s1", Some("/work/a"), 3_000, 4_000, 0.5)],
        );

        let found = collect(&store, &keys()).expect("collect");

        assert_eq!(found.sessions.len(), 1);
        let merged = &found.sessions[0];
        assert_eq!((merged.first_ms, merged.last_ms), (1_000, 4_000));
        assert!((merged.cost - 2.0).abs() < 1e-9);
        assert_eq!(merged.days, 2);
        assert_eq!(merged.project.as_deref(), Some("/work/a"));
    }

    #[test]
    fn the_same_session_id_on_two_machines_or_agents_stays_separate() {
        let store = MemoryStore::new();
        upload(
            &store,
            "m1",
            "claude",
            (2026, 10, 10),
            vec![session("same", None, 1, 2, 1.0)],
        );
        upload(
            &store,
            "m2",
            "claude",
            (2026, 10, 10),
            vec![session("same", None, 1, 2, 2.0)],
        );
        upload(
            &store,
            "m1",
            "codex",
            (2026, 10, 10),
            vec![session("same", None, 1, 2, 4.0)],
        );

        let found = collect(&store, &keys()).expect("collect");

        assert_eq!(found.sessions.len(), 3);
        let total: f64 = found.sessions.iter().map(|session| session.cost).sum();
        assert!((total - 7.0).abs() < 1e-9);
    }

    #[test]
    fn sessions_come_newest_first() {
        let store = MemoryStore::new();
        upload(
            &store,
            "m1",
            "claude",
            (2026, 10, 10),
            vec![
                session("old", None, 1, 10, 1.0),
                session("new", None, 1, 99, 1.0),
                session("mid", None, 1, 50, 1.0),
            ],
        );

        let ids: Vec<_> = collect(&store, &keys())
            .expect("collect")
            .sessions
            .into_iter()
            .map(|session| session.id)
            .collect();

        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    #[test]
    fn a_shard_that_cannot_be_read_is_skipped_rather_than_failing_the_dashboard() {
        let store = MemoryStore::new();
        upload(
            &store,
            "m1",
            "claude",
            (2026, 10, 10),
            vec![session("fine", None, 1, 2, 1.0)],
        );
        let broken = keys()
            .shard(
                USER,
                "m1",
                "claude",
                UtcDate::new(2026, 10, 11).expect("date"),
            )
            .expect("key");
        store.seed(&broken, b"this is not a shard");

        let found = collect(&store, &keys()).expect("collect");

        assert_eq!(found.sessions.len(), 1);
    }

    #[test]
    fn the_list_is_cut_and_says_so() {
        let sessions = Sessions {
            sessions: (0..3)
                .map(|n| SessionSummary {
                    machine: "m".to_string(),
                    agent: "a".to_string(),
                    id: n.to_string(),
                    project: None,
                    first_ms: 0,
                    last_ms: 0,
                    cost: 0.0,
                    days: 1,
                })
                .collect(),
            total: 10,
        };

        let document = to_json(&sessions, "2026-10-10T12:00:00Z");

        assert_eq!(document["truncated"], json!(true));
        assert_eq!(document["total"], json!(10));
        assert_eq!(document["sessions"].as_array().expect("array").len(), 3);
    }
}
