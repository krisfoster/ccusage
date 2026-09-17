//! The two values a bucket hands to every machine that joins it: the user ID
//! and the hash salt.
//!
//! Both are created once, by whichever machine sets the bucket up first, and
//! adopted verbatim by every machine afterwards. Both are written with
//! `IfAbsent` so two machines running `sync setup` at the same moment cannot
//! each mint their own — the loser of the race re-reads and adopts. Getting
//! this wrong is not a cosmetic problem: two user IDs split one person's usage
//! into two datasets, and two salts make the dedupe keys of the two machines
//! disjoint, so shared activity is counted twice.

use ccusage_objectstore::{KeySpace, ObjectStore, ObjectStoreError, Precondition};
use ccusage_sync::salt::{Salt, SaltOrigin, reconcile};
use serde_json::{Value, json};

/// The manifest shape this build creates.
const MANIFEST_SCHEMA: u32 = 1;
/// Bounded so a bucket that never settles fails loudly instead of spinning.
const MAX_ATTEMPTS: usize = 5;

pub(crate) type Result<T> = std::result::Result<T, String>;

/// Reads the bucket's salt, or writes the one this machine is going to use.
pub(crate) fn ensure_salt(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    configured: Option<&str>,
) -> Result<(Salt, SaltOrigin)> {
    let key = keys.salt();
    if let Some(existing) = read_field(store, &key, "salt")? {
        return reconcile(Some(&existing), configured).map_err(|error| error.to_string());
    }
    let (salt, origin) = reconcile(None, configured).map_err(|error| error.to_string())?;
    let body = json!({ "schema": MANIFEST_SCHEMA, "salt": salt.expose() });
    match store.put(
        &key,
        body.to_string().as_bytes(),
        "application/json",
        &Precondition::IfAbsent,
    ) {
        Ok(_) => Ok((salt, origin)),
        // Another machine set the bucket up between the read and the write; its
        // salt is the bucket's salt, and minting a second one here would make
        // the two machines' hashes disjoint forever.
        Err(ObjectStoreError::Conflict { .. }) => {
            let existing = read_field(store, &key, "salt")?
                .ok_or_else(|| "the bucket's salt vanished mid-setup; re-run setup".to_string())?;
            reconcile(Some(&existing), configured).map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The user ID recorded in the bucket, if the bucket has been set up before.
///
/// A second machine adopts this rather than minting its own, which is what
/// makes "point machine two at the same bucket" the whole of joining.
pub(crate) fn manifest_user_id(store: &dyn ObjectStore, keys: &KeySpace) -> Result<Option<String>> {
    read_field(store, &keys.manifest(), "userId")
}

/// Every machine registered in the bucket, in roster order.
///
/// Rollups read this rather than listing the bucket: a list is eventually
/// consistent about prefixes it has never seen, while the roster is written
/// with compare-and-swap, so a machine that finished registering is visible to
/// the next sync that runs anywhere.
pub(crate) fn manifest_machines(store: &dyn ObjectStore, keys: &KeySpace) -> Result<Vec<String>> {
    let key = keys.manifest();
    let Some((body, _)) = store.get(&key).map_err(|error| error.to_string())? else {
        return Ok(Vec::new());
    };
    let document: Value = serde_json::from_slice(&body)
        .map_err(|error| format!("{} is not readable JSON: {error}", key.path()))?;
    Ok(document
        .get("machines")
        .and_then(Value::as_array)
        .map(|machines| {
            machines
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Records the user ID for later machines. Never overwrites: the manifest is a
/// multi-writer object and rewriting it here would clobber a concurrent setup.
pub(crate) fn ensure_manifest(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
) -> Result<()> {
    let key = keys.manifest();
    if manifest_user_id(store, keys)?.is_some() {
        return Ok(());
    }
    let body = json!({ "schema": MANIFEST_SCHEMA, "userId": user_id });
    match store.put(
        &key,
        body.to_string().as_bytes(),
        "application/json",
        &Precondition::IfAbsent,
    ) {
        Ok(_) | Err(ObjectStoreError::Conflict { .. }) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Adds this machine to the manifest's roster so other machines — and the
/// dashboard — know it exists.
///
/// Unlike the machine's own objects, the manifest has as many writers as the
/// user has machines, so the write carries the generation it was read at and a
/// lost race is re-applied to the roster that won. Overwriting instead would
/// delete a machine that registered a moment earlier, and a machine missing
/// from the roster is a machine whose shards nothing reads.
pub(crate) fn register_machine(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    user_id: &str,
    machine_id: &str,
) -> Result<()> {
    let key = keys.manifest();
    for _ in 0..MAX_ATTEMPTS {
        let (mut document, generation) = match store.get(&key).map_err(|error| error.to_string())? {
            Some((body, meta)) => (
                serde_json::from_slice::<Value>(&body)
                    .map_err(|error| format!("{} is not readable JSON: {error}", key.path()))?,
                meta.generation,
            ),
            None => (
                json!({ "schema": MANIFEST_SCHEMA, "userId": user_id }),
                None,
            ),
        };
        let machines = document
            .get("machines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if machines
            .iter()
            .any(|entry| entry.as_str() == Some(machine_id))
        {
            return Ok(());
        }
        let mut machines: Vec<String> = machines
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        machines.push(machine_id.to_string());
        machines.sort();
        document["machines"] = json!(machines);

        let precondition = match &generation {
            Some(generation) => Precondition::IfGenerationMatch(generation.clone()),
            None => Precondition::IfAbsent,
        };
        match store.put(
            &key,
            document.to_string().as_bytes(),
            "application/json",
            &precondition,
        ) {
            Ok(_) => return Ok(()),
            Err(ObjectStoreError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(
        "the bucket manifest kept changing while registering this machine; re-run setup."
            .to_string(),
    )
}

/// Removes a machine from the roster, with the same merge-on-conflict rules
/// registering uses: a lost race must not resurrect it or drop a sibling.
pub(crate) fn unregister_machine(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    machine_id: &str,
) -> Result<()> {
    let key = keys.manifest();
    for _ in 0..MAX_ATTEMPTS {
        let Some((body, meta)) = store.get(&key).map_err(|error| error.to_string())? else {
            return Ok(());
        };
        let mut document: Value = serde_json::from_slice(&body)
            .map_err(|error| format!("{} is not readable JSON: {error}", key.path()))?;
        let machines: Vec<String> = document
            .get("machines")
            .and_then(Value::as_array)
            .map(|machines| {
                machines
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|entry| *entry != machine_id)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        document["machines"] = json!(machines);

        let precondition = match meta.generation {
            Some(generation) => Precondition::IfGenerationMatch(generation),
            None => Precondition::None,
        };
        match store.put(
            &key,
            document.to_string().as_bytes(),
            "application/json",
            &precondition,
        ) {
            Ok(_) => return Ok(()),
            Err(ObjectStoreError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("the bucket manifest kept changing while removing this machine; try again.".to_string())
}

fn read_field(
    store: &dyn ObjectStore,
    key: &ccusage_objectstore::Key,
    field: &str,
) -> Result<Option<String>> {
    let Some((body, _)) = store.get(key).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let document: Value = serde_json::from_slice(&body)
        .map_err(|error| format!("{} is not readable JSON: {error}", key.path()))?;
    Ok(document
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as MutCell;

    use ccusage_objectstore::{Key, ObjectMeta};
    use ccusage_test_support::objectstore::MemoryStore;

    use super::*;

    const SALT: &str = "00112233445566778899aabbccddeeff";
    const OTHER_SALT: &str = "ffeeddccbbaa99887766554433221100";

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    fn seeded_salt(store: &MemoryStore, keys: &KeySpace, salt: &str) {
        store
            .put(
                &keys.salt(),
                json!({ "schema": 1, "salt": salt }).to_string().as_bytes(),
                "application/json",
                &Precondition::None,
            )
            .expect("seed");
    }

    #[test]
    fn the_first_machine_mints_and_publishes_a_salt() {
        let store = MemoryStore::new();
        let keys = keys();

        let (salt, origin) = ensure_salt(&store, &keys, None).expect("bootstrap");

        assert_eq!(origin, SaltOrigin::Minted);
        let (published, _) = store.get(&keys.salt()).expect("get").expect("written");
        let document: Value = serde_json::from_slice(&published).expect("json");
        assert_eq!(document["salt"], salt.expose());
    }

    #[test]
    fn a_second_machine_adopts_the_bucket_salt_rather_than_minting_one() {
        let store = MemoryStore::new();
        let keys = keys();
        seeded_salt(&store, &keys, SALT);

        let (salt, origin) = ensure_salt(&store, &keys, None).expect("bootstrap");

        assert_eq!(salt.expose(), SALT);
        assert_eq!(origin, SaltOrigin::Bucket);
    }

    /// Hashing with a salt the bucket does not know produces keys that never
    /// intersect, so every entry both machines saw would be counted twice.
    #[test]
    fn a_config_salt_that_contradicts_the_bucket_stops_setup() {
        let store = MemoryStore::new();
        let keys = keys();
        seeded_salt(&store, &keys, SALT);

        let error = ensure_salt(&store, &keys, Some(OTHER_SALT)).expect_err("a mismatch is fatal");

        assert!(
            error.contains("not the one the bucket was set up with"),
            "{error}"
        );
    }

    /// A store where another machine writes the salt between this machine's
    /// read and its write — the window `IfAbsent` exists to close.
    struct RaceLostStore {
        inner: MemoryStore,
        keys: KeySpace,
        raced: MutCell<bool>,
    }

    impl ObjectStore for RaceLostStore {
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
                seeded_salt(&self.inner, &self.keys, SALT);
                let _ = self.inner.put(
                    &self.keys.manifest(),
                    json!({ "schema": 1, "userId": "u-1", "machines": ["machine-racer"] })
                        .to_string()
                        .as_bytes(),
                    "application/json",
                    &Precondition::None,
                );
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

    /// Two machines setting the bucket up at the same moment must not end with
    /// one of them using a salt the bucket never stored.
    #[test]
    fn losing_the_creation_race_adopts_the_winners_salt() {
        let keys = keys();
        let store = RaceLostStore {
            inner: MemoryStore::new(),
            keys: keys.clone(),
            raced: MutCell::new(false),
        };

        let (salt, origin) = ensure_salt(&store, &keys, None).expect("bootstrap");

        assert_eq!(salt.expose(), SALT);
        assert_eq!(origin, SaltOrigin::Bucket);
    }

    #[test]
    fn the_manifest_carries_the_user_id_to_the_next_machine() {
        let store = MemoryStore::new();
        let keys = keys();

        ensure_manifest(&store, &keys, "u-1234").expect("write");

        assert_eq!(
            manifest_user_id(&store, &keys).expect("read"),
            Some("u-1234".to_string())
        );
    }

    #[test]
    fn a_rerun_of_setup_leaves_an_existing_user_id_alone() {
        let store = MemoryStore::new();
        let keys = keys();
        ensure_manifest(&store, &keys, "u-first").expect("write");

        ensure_manifest(&store, &keys, "u-second").expect("write");

        assert_eq!(
            manifest_user_id(&store, &keys).expect("read"),
            Some("u-first".to_string())
        );
    }

    #[test]
    fn registering_a_machine_creates_the_roster_and_is_idempotent() {
        let store = MemoryStore::new();
        let keys = keys();

        register_machine(&store, &keys, "u-1", "machine-1").expect("first");
        register_machine(&store, &keys, "u-1", "machine-2").expect("second");
        register_machine(&store, &keys, "u-1", "machine-1").expect("repeat");

        let (body, _) = store.get(&keys.manifest()).expect("get").expect("written");
        let document: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(document["machines"], json!(["machine-1", "machine-2"]));
        assert_eq!(document["userId"], "u-1");
    }

    /// The manifest has one writer per machine, so a lost race must merge.
    #[test]
    fn registering_a_machine_keeps_one_registered_concurrently() {
        let keys = keys();
        let store = RaceLostStore {
            inner: MemoryStore::new(),
            keys: keys.clone(),
            raced: MutCell::new(false),
        };
        store
            .inner
            .put(
                &keys.manifest(),
                json!({ "schema": 1, "userId": "u-1", "machines": [] })
                    .to_string()
                    .as_bytes(),
                "application/json",
                &Precondition::None,
            )
            .expect("seed");

        register_machine(&store, &keys, "u-1", "machine-1").expect("register");

        let (body, _) = store.get(&keys.manifest()).expect("get").expect("written");
        let document: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(document["machines"], json!(["machine-1", "machine-racer"]));
    }

    #[test]
    fn an_unreadable_manifest_is_an_error_rather_than_a_fresh_identity() {
        let store = MemoryStore::new();
        let keys = keys();
        store
            .put(
                &keys.manifest(),
                b"not json",
                "application/json",
                &Precondition::None,
            )
            .expect("seed");

        assert!(manifest_user_id(&store, &keys).is_err());
    }
}
