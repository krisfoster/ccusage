//! An `ObjectStore` backed by a directory, for syncing without a cloud bucket.
//!
//! Each object is one plain file at its key's path under the root, so the data stays readable
//! with `cat` and survives the tool. Two properties make it a valid store for the sync engine:
//!
//! - A generation is the SHA-256 of the object's body. Every change to the content changes the
//!   generation, which is all compare-and-swap needs: a writer holding a stale generation holds a
//!   stale merge, and rewriting identical bytes cannot invalidate a merge that was based on them.
//! - Writes are serialised by an advisory lock on one file at the root, and each write is
//!   temp-file-then-rename. The lock makes check-then-write atomic across threads and across
//!   processes sharing the directory; the rename means a reader never sees half an object.
//!
//! Content types are not stored. Reads are unlocked and rely on the rename.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use ccusage_objectstore::{
    Key, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, Result, sha256_hex,
};

/// `~` cannot appear in a validated key segment, so neither name can collide with an object.
const LOCK_FILE: &str = "~lock";
const TEMP_PREFIX: &str = "~tmp-";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, key: &Key) -> PathBuf {
        // Key segments are validated at construction, so this cannot leave the root.
        self.root.join(key.path())
    }

    fn lock(&self, key: &str) -> Result<File> {
        fs::create_dir_all(&self.root).map_err(|error| io_error(key, &error))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(LOCK_FILE))
            .map_err(|error| io_error(key, &error))?;
        file.lock().map_err(|error| io_error(key, &error))?;
        Ok(file)
    }
}

impl ObjectStore for FsStore {
    fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, ObjectMeta)>> {
        let path = self.object_path(key);
        let Some(body) = read_body(&path, key.path())? else {
            return Ok(None);
        };
        let meta = meta_for(key.path(), &body, modified_ms(&path));
        Ok(Some((body, meta)))
    }

    fn put(
        &self,
        key: &Key,
        body: &[u8],
        _content_type: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta> {
        let path = self.object_path(key);
        let _lock = self.lock(key.path())?;
        let current = read_body(&path, key.path())?.map(|existing| sha256_hex(&existing));
        check(precondition, key.path(), current.as_deref())?;
        write_atomic(&path, body, key.path())?;
        Ok(meta_for(key.path(), body, modified_ms(&path)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        if !listable_prefix(prefix) {
            // No validated key can start with this, so nothing can match.
            return Ok(Vec::new());
        }
        // Start below the deepest directory the prefix names rather than at the root.
        let start = match prefix.rfind('/') {
            Some(slash) => self.root.join(&prefix[..slash]),
            None => self.root.clone(),
        };
        let mut found = Vec::new();
        walk(&start, &self.root, prefix, &mut found)?;
        found.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(found)
    }

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()> {
        let path = self.object_path(key);
        let _lock = self.lock(key.path())?;
        let Some(existing) = read_body(&path, key.path())? else {
            // Deleting what is not there succeeds, as it does on GCS.
            return Ok(());
        };
        check(precondition, key.path(), Some(&sha256_hex(&existing)))?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(key.path(), &error)),
        }
    }
}

fn check(precondition: &Precondition, key: &str, current: Option<&str>) -> Result<()> {
    let satisfied = match precondition {
        Precondition::None => true,
        Precondition::IfAbsent => current.is_none(),
        Precondition::IfGenerationMatch(expected) => current == Some(expected.as_str()),
    };
    if satisfied {
        Ok(())
    } else {
        Err(ObjectStoreError::Conflict {
            key: key.to_string(),
        })
    }
}

/// A prefix is a string match against key paths, so the only prefixes worth walking the disk
/// for are ones a validated key could start with. Anything else could only escape the root.
fn listable_prefix(prefix: &str) -> bool {
    prefix.split('/').all(|segment| {
        segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && !(!segment.is_empty() && segment.bytes().all(|byte| byte == b'.'))
    })
}

fn walk(dir: &Path, root: &Path, prefix: &str, found: &mut Vec<ObjectMeta>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(prefix, &error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| io_error(prefix, &error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == LOCK_FILE || name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .filter_map(|part| part.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        let file_type = entry
            .file_type()
            .map_err(|error| io_error(prefix, &error))?;
        if file_type.is_dir() {
            // A directory can only lead to matches if it and the prefix agree as far as
            // the shorter of the two goes.
            let directory = format!("{relative}/");
            if directory.starts_with(prefix) || prefix.starts_with(&directory) {
                walk(&path, root, prefix, found)?;
            }
        } else if relative.starts_with(prefix)
            && let Some(body) = read_body(&path, &relative)?
        {
            found.push(meta_for(&relative, &body, modified_ms(&path)));
        }
    }
    Ok(())
}

fn read_body(path: &Path, key: &str) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(body) => Ok(Some(body)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // A key that is a prefix of other keys is a directory, which holds no object of its own.
        Err(_) if path.is_dir() => Ok(None),
        Err(error) => Err(io_error(key, &error)),
    }
}

fn write_atomic(path: &Path, body: &[u8], key: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| ObjectStoreError::Other {
        detail: format!("{key} has no parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error(key, &error))?;
    let temp = parent.join(format!(
        "{TEMP_PREFIX}{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = File::create(&temp)
        .and_then(|mut file| {
            file.write_all(body)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_error(key, &error));
    }
    Ok(())
}

fn meta_for(key: &str, body: &[u8], updated_ms: Option<i64>) -> ObjectMeta {
    let generation = sha256_hex(body);
    ObjectMeta {
        key: key.to_string(),
        etag: Some(format!("\"{generation}\"")),
        generation: Some(generation),
        size: body.len() as u64,
        updated_ms,
    }
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_millis()).ok()
}

fn io_error(key: &str, error: &io::Error) -> ObjectStoreError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ObjectStoreError::Forbidden {
            key: key.to_string(),
            detail: error.to_string(),
        },
        _ => ObjectStoreError::Other {
            detail: format!("{key}: {error}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use assert_fs::TempDir;
    use ccusage_objectstore::KeySpace;

    use super::*;

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("key space")
    }

    fn store() -> (TempDir, FsStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = FsStore::new(dir.path().join("store"));
        (dir, store)
    }

    fn put(
        store: &FsStore,
        key: &Key,
        body: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta> {
        store.put(key, body.as_bytes(), "application/json", precondition)
    }

    #[test]
    fn writes_and_reads_back_an_object_with_a_content_generation() {
        let (_dir, store) = store();
        let key = keys().manifest();

        let written = put(&store, &key, r#"{"a":1}"#, &Precondition::IfAbsent).expect("put");
        let (body, meta) = store.get(&key).expect("get").expect("object exists");

        assert_eq!(body, br#"{"a":1}"#);
        assert_eq!(meta.generation, written.generation);
        assert_eq!(meta.generation.as_deref(), Some(sha256_hex(&body).as_str()));
        assert_eq!(meta.size, 7);
        assert!(meta.updated_ms.is_some());
    }

    #[test]
    fn stores_each_object_as_a_plain_file_at_its_key() {
        let (_dir, store) = store();
        let key = keys().manifest();
        put(&store, &key, "hello", &Precondition::None).expect("put");

        let on_disk =
            fs::read_to_string(store.root().join("ccusage/v1/manifest.json")).expect("file");
        assert_eq!(on_disk, "hello");
    }

    #[test]
    fn reports_a_missing_object_as_none() {
        let (_dir, store) = store();
        assert!(store.get(&keys().manifest()).expect("get").is_none());
    }

    #[test]
    fn if_absent_refuses_to_overwrite() {
        let (_dir, store) = store();
        let key = keys().salt();
        put(&store, &key, "one", &Precondition::IfAbsent).expect("first put");

        let second = put(&store, &key, "two", &Precondition::IfAbsent);
        assert!(matches!(second, Err(ObjectStoreError::Conflict { .. })));
        assert_eq!(store.get(&key).unwrap().unwrap().0, b"one");
    }

    #[test]
    fn generation_match_accepts_the_current_generation_and_rejects_a_stale_one() {
        let (_dir, store) = store();
        let key = keys().manifest();
        let first = put(&store, &key, "one", &Precondition::None).expect("put");
        let generation = first.generation.expect("generation");

        let second = put(
            &store,
            &key,
            "two",
            &Precondition::IfGenerationMatch(generation.clone()),
        )
        .expect("current generation is accepted");
        assert_ne!(second.generation.as_deref(), Some(generation.as_str()));

        let stale = put(
            &store,
            &key,
            "three",
            &Precondition::IfGenerationMatch(generation),
        );
        assert!(matches!(stale, Err(ObjectStoreError::Conflict { .. })));
        assert_eq!(store.get(&key).unwrap().unwrap().0, b"two");
    }

    #[test]
    fn generation_match_on_a_missing_object_conflicts() {
        let (_dir, store) = store();
        let result = put(
            &store,
            &keys().manifest(),
            "x",
            &Precondition::IfGenerationMatch("abc".to_string()),
        );
        assert!(matches!(result, Err(ObjectStoreError::Conflict { .. })));
    }

    #[test]
    fn a_second_store_on_the_same_directory_sees_the_data() {
        let (dir, store) = store();
        let key = keys().manifest();
        put(&store, &key, "kept", &Precondition::None).expect("put");

        let reopened = FsStore::new(dir.path().join("store"));
        assert_eq!(reopened.get(&key).unwrap().unwrap().0, b"kept");
    }

    #[test]
    fn lists_by_string_prefix_in_key_order_and_skips_bookkeeping_files() {
        let (_dir, store) = store();
        let space = keys();
        let a = space
            .shard(
                "u1",
                "m1",
                "claude",
                ccusage_objectstore::UtcDate::new(2026, 10, 9).unwrap(),
            )
            .unwrap();
        let b = space
            .shard(
                "u1",
                "m1",
                "codex",
                ccusage_objectstore::UtcDate::new(2026, 10, 9).unwrap(),
            )
            .unwrap();
        let other = space
            .shard(
                "u1",
                "m2",
                "claude",
                ccusage_objectstore::UtcDate::new(2026, 10, 9).unwrap(),
            )
            .unwrap();
        for key in [&b, &other, &a, &space.manifest()] {
            put(&store, key, "{}", &Precondition::None).expect("put");
        }
        fs::write(store.root().join("ccusage/v1/~tmp-1-1"), "partial").unwrap();

        let m1 = store
            .list(&space.machine_prefix("u1", "m1").unwrap())
            .expect("list");
        assert_eq!(
            m1.iter().map(|meta| meta.key.as_str()).collect::<Vec<_>>(),
            vec![a.path(), b.path()]
        );

        let everything = store.list("").expect("list all");
        assert_eq!(everything.len(), 4);
        assert!(everything.iter().all(|meta| !meta.key.contains('~')));

        // A prefix that stops mid-name still matches by string, as it does on a bucket.
        let partial = store.list("ccusage/v1/man").expect("list partial");
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].key, "ccusage/v1/manifest.json");
    }

    #[test]
    fn list_never_leaves_the_root() {
        let (dir, store) = store();
        fs::write(dir.path().join("outside.json"), "secret").unwrap();
        put(&store, &keys().manifest(), "{}", &Precondition::None).expect("put");

        for prefix in ["../", "..", "ccusage/../../", "/etc", "a\\b"] {
            assert!(
                store.list(prefix).expect("list").is_empty(),
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn delete_removes_an_object_and_tolerates_a_missing_one() {
        let (_dir, store) = store();
        let key = keys().manifest();
        put(&store, &key, "x", &Precondition::None).expect("put");

        store.delete(&key, &Precondition::None).expect("delete");
        assert!(store.get(&key).unwrap().is_none());
        store
            .delete(&key, &Precondition::None)
            .expect("deleting again is fine");
    }

    #[test]
    fn delete_honours_a_generation_precondition() {
        let (_dir, store) = store();
        let key = keys().manifest();
        let meta = put(&store, &key, "x", &Precondition::None).expect("put");

        let stale = store.delete(&key, &Precondition::IfGenerationMatch("nope".to_string()));
        assert!(matches!(stale, Err(ObjectStoreError::Conflict { .. })));
        assert!(store.get(&key).unwrap().is_some());

        store
            .delete(
                &key,
                &Precondition::IfGenerationMatch(meta.generation.unwrap()),
            )
            .expect("delete at the current generation");
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn a_key_that_is_a_prefix_of_other_keys_reads_as_absent() {
        let (_dir, store) = store();
        let space = keys();
        put(
            &store,
            &space
                .shard(
                    "u1",
                    "m1",
                    "claude",
                    ccusage_objectstore::UtcDate::new(2026, 1, 2).unwrap(),
                )
                .unwrap(),
            "{}",
            &Precondition::None,
        )
        .unwrap();

        let directory = space.listed("ccusage/v1/users/u1").expect("key");
        assert!(store.get(&directory).expect("get").is_none());
    }

    /// The property the sync engine depends on: concurrent read-merge-write loops lose no update.
    /// Each thread opens its own store on the directory, so the lock is exercised the way two
    /// processes would exercise it.
    #[test]
    fn concurrent_compare_and_swap_loops_lose_no_updates() {
        const THREADS: usize = 8;
        const INCREMENTS: usize = 25;

        let dir = Arc::new(TempDir::new().expect("temp dir"));
        let root = dir.path().join("store");
        let key = keys().rollup(ccusage_objectstore::RollupKind::Daily);
        FsStore::new(&root)
            .put(&key, b"0", "text/plain", &Precondition::IfAbsent)
            .expect("seed");

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let root = root.clone();
                let key = key.clone();
                thread::spawn(move || {
                    let store = FsStore::new(root);
                    for _ in 0..INCREMENTS {
                        loop {
                            let (body, meta) = store.get(&key).unwrap().unwrap();
                            let next = String::from_utf8(body).unwrap().parse::<u64>().unwrap() + 1;
                            let precondition =
                                Precondition::IfGenerationMatch(meta.generation.unwrap());
                            match store.put(
                                &key,
                                next.to_string().as_bytes(),
                                "text/plain",
                                &precondition,
                            ) {
                                Ok(_) => break,
                                Err(ObjectStoreError::Conflict { .. }) => continue,
                                Err(error) => panic!("unexpected error: {error}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker finished");
        }

        let (body, _) = FsStore::new(&root).get(&key).unwrap().unwrap();
        assert_eq!(body, (THREADS * INCREMENTS).to_string().as_bytes());
    }

    #[test]
    fn concurrent_if_absent_has_exactly_one_winner() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("store");
        let key = keys().salt();

        let handles: Vec<_> = (0..8)
            .map(|index| {
                let root = root.clone();
                let key = key.clone();
                thread::spawn(move || {
                    FsStore::new(root)
                        .put(
                            &key,
                            format!("writer-{index}").as_bytes(),
                            "text/plain",
                            &Precondition::IfAbsent,
                        )
                        .is_ok()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker finished"))
            .filter(|won| *won)
            .count();

        assert_eq!(winners, 1);
    }

    #[test]
    fn passes_the_doctor_checks_a_real_store_must_pass() {
        use crate::sync::doctor::{Outcome, run_checks};

        let (_dir, store) = store();
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("fits");
        let checks = run_checks(&store, &keys(), "machine-1", now_ms);

        let failures: Vec<_> = checks
            .iter()
            .filter(|check| check.outcome == Outcome::Fail)
            .map(|check| check.name)
            .collect();
        assert!(failures.is_empty(), "failed checks: {failures:?}");
    }
}
