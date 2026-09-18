//! The double has to be trustworthy before anything is tested against it, so its CAS and fault
//! behavior is pinned here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ccusage_objectstore::{Key, KeySpace, ObjectStore, ObjectStoreError, Precondition, RollupKind};
use ccusage_test_support::objectstore::{Fault, MemoryStore, Op, When};

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

#[test]
fn a_keyed_fault_spares_every_other_object() {
    let store = MemoryStore::new();
    let keys = KeySpace::new("ccusage/v1").unwrap();
    store.fail_on(When::put("daily.json"), Fault::Network);

    let other = store.put(
        &keys.manifest(),
        b"{}",
        "application/json",
        &Precondition::None,
    );
    let daily = store.put(
        &keys.rollup(RollupKind::Daily),
        b"{}",
        "application/json",
        &Precondition::None,
    );

    assert!(other.is_ok());
    assert!(matches!(daily, Err(ObjectStoreError::Network { .. })));
}

#[test]
fn skip_lets_the_first_writes_through_and_fails_the_one_named() {
    let store = MemoryStore::new();
    let keys = KeySpace::new("ccusage/v1").unwrap();
    store.fail_on(When::put("").skip(2), Fault::Server { status: 503 });

    let results: Vec<_> = (0..4)
        .map(|n| {
            let key = keys.rollup(if n % 2 == 0 {
                RollupKind::Daily
            } else {
                RollupKind::Weekly
            });
            store
                .put(&key, b"{}", "application/json", &Precondition::None)
                .is_ok()
        })
        .collect();

    assert_eq!(results, vec![true, true, false, true]);
}

/// The case a naive retry turns into a duplicate: the object is written and
/// the caller is told it was not.
#[test]
fn a_lost_response_still_writes_the_object() {
    let store = MemoryStore::new();
    let key = manifest();
    store.lose_response_on(When::put("manifest"), Fault::Network);

    let result = store.put(&key, b"{\"v\":1}", "application/json", &Precondition::None);

    assert!(matches!(result, Err(ObjectStoreError::Network { .. })));
    assert_eq!(store.body(key.path()).as_deref(), Some(&b"{\"v\":1}"[..]));
}

/// The hook is the second process's turn, so it has to see the store as it is
/// immediately before the first process's write.
#[test]
fn a_hook_runs_before_the_request_it_is_attached_to() {
    let store = Arc::new(MemoryStore::new());
    let key = manifest();
    let observer = Arc::clone(&store);
    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    store.before(When::put("manifest").skip(1), move || {
        *record.lock().unwrap() = observer.body(manifest().path());
    });

    store
        .put(&key, b"first", "application/json", &Precondition::None)
        .unwrap();
    store
        .put(&key, b"second", "application/json", &Precondition::None)
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_deref(), Some(&b"first"[..]));
    assert_eq!(store.body(key.path()).as_deref(), Some(&b"second"[..]));
}

#[test]
fn counts_requests_per_operation_and_key() {
    let store = MemoryStore::new();
    let key = manifest();
    store
        .put(&key, b"{}", "application/json", &Precondition::None)
        .unwrap();
    store.get(&key).unwrap();
    store.get(&key).unwrap();

    assert_eq!(store.op_count(Op::Get, "manifest"), 2);
    assert_eq!(store.op_count(Op::Put, "manifest"), 1);
    assert_eq!(store.op_count(Op::Get, "shards"), 0);

    store.reset_counts();
    assert_eq!(store.op_count(Op::Get, "manifest"), 0);
}
