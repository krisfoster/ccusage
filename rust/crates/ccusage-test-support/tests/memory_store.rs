//! The double has to be trustworthy before anything is tested against it, so its CAS and fault
//! behavior is pinned here.

use std::time::{Duration, Instant};

use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition, RollupKind};
use ccusage_test_support::objectstore::{Fault, MemoryStore};

fn manifest() -> Key {
    KeySpace::new("ccusage/v1").unwrap().manifest()
}

#[test]
fn reads_back_what_was_written() {
    let store = MemoryStore::new();
    let key = manifest();
    store
        .put(&key, b"{\"v\":1}", "application/json", &Precondition::None)
        .unwrap();

    let (body, meta) = store.get(&key).unwrap().unwrap();
    assert_eq!(body, b"{\"v\":1}");
    assert_eq!(meta.key, key.path());
    assert_eq!(meta.size, 7);
    assert_eq!(
        store.content_type(&key).as_deref(),
        Some("application/json")
    );
}

#[test]
fn reports_a_missing_object_as_none_rather_than_an_error() {
    assert!(MemoryStore::new().get(&manifest()).unwrap().is_none());
}

#[test]
fn changes_the_generation_on_every_write() {
    let store = MemoryStore::new();
    let key = manifest();
    let first = store
        .put(&key, b"a", "application/json", &Precondition::None)
        .unwrap();
    let second = store
        .put(&key, b"b", "application/json", &Precondition::None)
        .unwrap();
    assert_ne!(first.generation, second.generation);
    assert!(first.generation.is_some());
}

#[test]
fn if_absent_admits_only_the_first_writer() {
    let store = MemoryStore::new();
    let key = manifest();
    store
        .put(&key, b"a", "application/json", &Precondition::IfAbsent)
        .unwrap();

    let err = store
        .put(&key, b"b", "application/json", &Precondition::IfAbsent)
        .unwrap_err();
    assert!(err.is_conflict(), "{err:?}");
    assert_eq!(store.get(&key).unwrap().unwrap().0, b"a");
}

#[test]
fn if_generation_match_rejects_a_write_built_on_a_stale_read() {
    let store = MemoryStore::new();
    let key = manifest();
    let stale = store
        .put(&key, b"a", "application/json", &Precondition::None)
        .unwrap()
        .generation
        .unwrap();
    // Another machine writes in between.
    store
        .put(&key, b"b", "application/json", &Precondition::None)
        .unwrap();

    let err = store
        .put(
            &key,
            b"c",
            "application/json",
            &Precondition::IfGenerationMatch(stale.clone()),
        )
        .unwrap_err();
    assert!(err.is_conflict(), "{err:?}");
    assert!(!err.is_transport_retryable());
    assert_eq!(
        store.get(&key).unwrap().unwrap().0,
        b"b",
        "the losing write must not land"
    );

    let fresh = store.get(&key).unwrap().unwrap().1.generation.unwrap();
    store
        .put(
            &key,
            b"c",
            "application/json",
            &Precondition::IfGenerationMatch(fresh),
        )
        .expect("re-reading and retrying should succeed");
}

#[test]
fn lists_only_keys_under_the_requested_prefix() {
    let space = KeySpace::new("ccusage/v1").unwrap();
    let store = MemoryStore::new();
    let mine = space.machine("u1", "m1").unwrap();
    let theirs = space.machine("u1", "m2").unwrap();
    store.seed(&mine, b"{}");
    store.seed(&theirs, b"{}");
    store.seed(&space.rollup(RollupKind::Daily), b"{}");

    let listed = store
        .list(&space.machine_prefix("u1", "m1").unwrap())
        .unwrap();
    assert_eq!(
        listed.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
        vec![mine.path()]
    );
}

#[test]
fn deletes_only_at_the_expected_generation() {
    let store = MemoryStore::new();
    let key = manifest();
    let stale = store.seed(&key, b"a").generation.unwrap();
    store.seed(&key, b"b");

    let err = store
        .delete(&key, &Precondition::IfGenerationMatch(stale))
        .unwrap_err();
    assert!(err.is_conflict(), "{err:?}");
    assert!(store.contains(&key));

    store.delete(&key, &Precondition::None).unwrap();
    assert!(!store.contains(&key));
    assert!(matches!(
        store.delete(&key, &Precondition::None).unwrap_err(),
        ObjectStoreError::NotFound { .. }
    ));
}

#[test]
fn returns_queued_faults_in_order_then_succeeds() {
    let store = MemoryStore::new();
    let key = manifest();
    store.fail_next(Fault::Server { status: 503 });
    store.fail_next(Fault::RateLimited {
        retry_after_ms: Some(250),
    });
    store.fail_next(Fault::Network);

    let first = store.get(&key).unwrap_err();
    assert!(matches!(
        first,
        ObjectStoreError::Server { status: 503, .. }
    ));
    let second = store.get(&key).unwrap_err();
    assert_eq!(second.retry_after_ms(), Some(250));
    assert!(matches!(
        store.get(&key).unwrap_err(),
        ObjectStoreError::Network { .. }
    ));
    assert!(store.get(&key).unwrap().is_none());
    assert_eq!(store.request_count(), 4, "failed attempts still count");
}

#[test]
fn injects_a_conflict_without_touching_the_stored_object() {
    let store = MemoryStore::new();
    let key = manifest();
    store.seed(&key, b"a");
    store.fail_next(Fault::Conflict);

    assert!(
        store
            .put(&key, b"b", "application/json", &Precondition::None)
            .unwrap_err()
            .is_conflict()
    );
    assert_eq!(store.get(&key).unwrap().unwrap().0, b"a");
}

#[test]
fn delays_each_operation_by_the_configured_latency() {
    let store = MemoryStore::new().with_latency(Duration::from_millis(20));
    let started = Instant::now();
    store.get(&manifest()).unwrap();
    store.get(&manifest()).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(40));
}
