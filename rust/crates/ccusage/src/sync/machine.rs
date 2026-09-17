//! The two objects a machine owns outright: what it is, and what it has
//! uploaded.
//!
//! Both are single-writer — only this machine ever writes under its own prefix
//! — but "single writer" means one machine, not one process: two terminals
//! running `ccusage sync` at the same time would otherwise each write the index
//! they read, and the slower one would erase the other's shards from it. Every
//! write therefore carries the generation it was read at, and a lost race is
//! retried against the value that won rather than overwritten.

use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How many times a write may lose the generation race before giving up. A
/// single concurrent sync resolves on the first retry; more than this means
/// something is looping, and failing loudly beats spinning.
const MAX_ATTEMPTS: usize = 5;
const SCHEMA: u32 = 1;

pub(crate) type Result<T> = std::result::Result<T, String>;

/// What the dashboard needs to name a machine, and nothing that identifies the
/// hardware beyond the fingerprint already used to spot a copied config.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MachineRecord {
    pub schema: u32,
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Diagnostic only: it says "this config looks copied", never which machine
    /// a shard belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
}

/// One uploaded shard, as this machine last left it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndexEntry {
    pub content_hash: String,
    /// A finalized shard is not rewritten again, so a reader can cache it.
    #[serde(default)]
    pub finalized: bool,
    pub updated_at: String,
}

/// Which shards this machine has uploaded, keyed by `<agent>/<YYYY-MM-DD>`.
///
/// This is what makes an incremental sync possible: a shard whose freshly
/// computed content hash matches the index entry does not need uploading.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MachineIndex {
    pub schema: u32,
    #[serde(default)]
    pub shards: BTreeMap<String, IndexEntry>,
}

impl Default for MachineIndex {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            shards: BTreeMap::new(),
        }
    }
}

impl MachineIndex {
    pub fn entry_key(agent: &str, utc_date: &str) -> String {
        format!("{agent}/{utc_date}")
    }

    /// Whether a shard with this content hash is already in the bucket.
    pub fn is_current(&self, agent: &str, utc_date: &str, content_hash: &str) -> bool {
        self.shards
            .get(&Self::entry_key(agent, utc_date))
            .is_some_and(|entry| entry.content_hash == content_hash)
    }
}

/// Reads an object and the generation it was read at, so the write that follows
/// can prove nothing changed in between.
fn read<T: Default + for<'de> Deserialize<'de>>(
    store: &dyn ObjectStore,
    key: &Key,
) -> Result<(T, Option<String>)> {
    let Some((body, meta)) = store.get(key).map_err(|error| error.to_string())? else {
        return Ok((T::default(), None));
    };
    let value = serde_json::from_slice(&body)
        .map_err(|error| format!("{} is not readable: {error}", key.path()))?;
    Ok((value, meta.generation))
}

fn write<T: Serialize>(
    store: &dyn ObjectStore,
    key: &Key,
    value: &T,
    generation: Option<&str>,
) -> std::result::Result<(), ObjectStoreError> {
    let body = serde_json::to_vec(value).expect("the record is serializable");
    let precondition = match generation {
        Some(generation) => Precondition::IfGenerationMatch(generation.to_string()),
        None => Precondition::IfAbsent,
    };
    store
        .put(key, &body, "application/json", &precondition)
        .map(|_| ())
}

/// Applies `mutate` to the machine's index and stores the result, re-reading
/// and re-applying it if another process wrote first.
pub(crate) fn update_index(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    mut mutate: impl FnMut(&mut MachineIndex),
) -> Result<MachineIndex> {
    let key = keys
        .machine_index(user_id, machine_id)
        .map_err(|error| error.to_string())?;
    for _ in 0..MAX_ATTEMPTS {
        let (mut index, generation): (MachineIndex, _) = read(store, &key)?;
        index.schema = SCHEMA;
        mutate(&mut index);
        match write(store, &key, &index, generation.as_deref()) {
            Ok(()) => return Ok(index),
            Err(ObjectStoreError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "the index for machine {machine_id} kept changing under this sync. Re-run 'ccusage sync' once no other sync is running."
    ))
}

pub(crate) fn load_index(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
) -> Result<MachineIndex> {
    let key = keys
        .machine_index(user_id, machine_id)
        .map_err(|error| error.to_string())?;
    read(store, &key).map(|(index, _)| index)
}

/// Records what this machine is. Same race, same rule as the index.
pub(crate) fn update_machine(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
    mut mutate: impl FnMut(&mut MachineRecord),
) -> Result<MachineRecord> {
    let key = keys
        .machine(user_id, machine_id)
        .map_err(|error| error.to_string())?;
    for _ in 0..MAX_ATTEMPTS {
        let (mut record, generation): (MachineRecord, _) = read(store, &key)?;
        record.schema = SCHEMA;
        record.machine_id = machine_id.to_string();
        mutate(&mut record);
        match write(store, &key, &record, generation.as_deref()) {
            Ok(()) => return Ok(record),
            Err(ObjectStoreError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "the record for machine {machine_id} kept changing under this sync. Re-run 'ccusage sync' once no other sync is running."
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as MutCell;

    use ccusage_objectstore::ObjectMeta;
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;

    const USER: &str = "user-1";
    const MACHINE: &str = "machine-1";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    fn entry(hash: &str) -> IndexEntry {
        IndexEntry {
            content_hash: hash.to_string(),
            finalized: false,
            updated_at: "2026-09-17T18:12:03Z".to_string(),
        }
    }

    #[test]
    fn the_first_sync_creates_the_index() {
        let store = MemoryStore::new();
        let keys = keys();

        let index = update_index(&store, &keys, USER, MACHINE, |index| {
            index.shards.insert(
                MachineIndex::entry_key("claude", "2026-09-17"),
                entry("sha256:a"),
            );
        })
        .expect("update");

        assert_eq!(index.schema, SCHEMA);
        assert!(index.is_current("claude", "2026-09-17", "sha256:a"));
        assert_eq!(
            load_index(&store, &keys, USER, MACHINE).expect("load"),
            index
        );
    }

    #[test]
    fn a_later_sync_keeps_the_shards_an_earlier_one_recorded() {
        let store = MemoryStore::new();
        let keys = keys();
        update_index(&store, &keys, USER, MACHINE, |index| {
            index.shards.insert(
                MachineIndex::entry_key("claude", "2026-09-16"),
                entry("sha256:a"),
            );
        })
        .expect("first");

        let index = update_index(&store, &keys, USER, MACHINE, |index| {
            index.shards.insert(
                MachineIndex::entry_key("claude", "2026-09-17"),
                entry("sha256:b"),
            );
        })
        .expect("second");

        assert_eq!(index.shards.len(), 2);
    }

    #[test]
    fn an_unchanged_shard_is_recognized_from_its_hash() {
        let index = MachineIndex {
            schema: SCHEMA,
            shards: BTreeMap::from([(
                MachineIndex::entry_key("claude", "2026-09-17"),
                entry("sha256:a"),
            )]),
        };

        assert!(index.is_current("claude", "2026-09-17", "sha256:a"));
        assert!(!index.is_current("claude", "2026-09-17", "sha256:b"));
        assert!(!index.is_current("codex", "2026-09-17", "sha256:a"));
    }

    /// A store that lets another writer in exactly once between the read and
    /// the write, the way a second `ccusage sync` on the same machine would.
    struct RacingStore {
        inner: MemoryStore,
        keys: KeySpace,
        raced: MutCell<bool>,
    }

    impl RacingStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::new(),
                keys: keys(),
                raced: MutCell::new(false),
            }
        }
    }

    impl ObjectStore for RacingStore {
        fn get(&self, key: &Key) -> ccusage_objectstore::Result<Option<(Vec<u8>, ObjectMeta)>> {
            self.inner.get(key)
        }

        fn put(
            &self,
            key: &Key,
            body: &[u8],
            content_type: &str,
            precondition: &Precondition,
        ) -> ccusage_objectstore::Result<ObjectMeta> {
            if !self.raced.replace(true) {
                let other = MachineIndex {
                    schema: SCHEMA,
                    shards: BTreeMap::from([(
                        MachineIndex::entry_key("codex", "2026-09-17"),
                        entry("sha256:other"),
                    )]),
                };
                self.inner
                    .put(
                        &self.keys.machine_index(USER, MACHINE).expect("key"),
                        &serde_json::to_vec(&other).expect("json"),
                        "application/json",
                        &Precondition::None,
                    )
                    .expect("seed");
            }
            self.inner.put(key, body, content_type, precondition)
        }

        fn list(&self, prefix: &str) -> ccusage_objectstore::Result<Vec<ObjectMeta>> {
            self.inner.list(prefix)
        }

        fn delete(
            &self,
            key: &Key,
            precondition: &Precondition,
        ) -> ccusage_objectstore::Result<()> {
            self.inner.delete(key, precondition)
        }
    }

    /// Losing the race must merge with the winner, not erase it.
    #[test]
    fn a_concurrent_sync_does_not_lose_the_shards_it_recorded() {
        let store = RacingStore::new();
        let keys = keys();

        let index = update_index(&store, &keys, USER, MACHINE, |index| {
            index.shards.insert(
                MachineIndex::entry_key("claude", "2026-09-17"),
                entry("sha256:a"),
            );
        })
        .expect("update");

        assert!(index.is_current("claude", "2026-09-17", "sha256:a"));
        assert!(index.is_current("codex", "2026-09-17", "sha256:other"));
    }

    #[test]
    fn the_machine_record_carries_its_own_id_whatever_the_caller_sets() {
        let store = MemoryStore::new();
        let keys = keys();

        let record = update_machine(&store, &keys, USER, MACHINE, |record| {
            record.label = Some("laptop".to_string());
            record.last_sync_at = Some("2026-09-17T18:12:03Z".to_string());
        })
        .expect("update");

        assert_eq!(record.machine_id, MACHINE);
        assert_eq!(record.label.as_deref(), Some("laptop"));
    }

    #[test]
    fn an_unreadable_index_is_an_error_rather_than_a_silent_reset() {
        let store = MemoryStore::new();
        let keys = keys();
        store
            .put(
                &keys.machine_index(USER, MACHINE).expect("key"),
                b"not json",
                "application/json",
                &Precondition::None,
            )
            .expect("seed");

        assert!(load_index(&store, &keys, USER, MACHINE).is_err());
    }
}
