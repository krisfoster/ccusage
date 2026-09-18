//! An in-memory `ObjectStore`, so the sync engine's merge and retry logic can be tested without
//! a network or a bucket.
//!
//! Generations behave the way GCS's do — a monotonically increasing token per object, changed by
//! every successful write — because the CAS loops under test are only interesting when a second
//! writer can move the object between the read and the write. `fail_next` is what lets a test put
//! that second writer, or a 503, exactly where it wants it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ccusage_objectstore::{Key, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, Result};

/// An error to return instead of performing the next operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    RateLimited {
        retry_after_ms: Option<u64>,
    },
    Server {
        status: u16,
    },
    Network,
    Conflict,
    /// The credential expired mid-run.
    Unauthenticated,
}

/// Which kind of request a rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    Get,
    Put,
    List,
    Delete,
}

/// Picks the requests a rule fires on: an operation, a key substring, and how
/// many matching requests to let past first.
///
/// Substring rather than exact key, because tests care about "the third shard
/// write" far more often than about one spelled-out path, and those paths carry
/// hashed identifiers a test would otherwise have to reconstruct.
#[derive(Debug, Clone)]
pub struct When {
    op: Option<Op>,
    contains: String,
    skip: usize,
    times: usize,
}

impl When {
    /// Any operation whose key contains `contains`; `""` matches every key.
    pub fn any(contains: &str) -> Self {
        Self {
            op: None,
            contains: contains.to_string(),
            skip: 0,
            times: 1,
        }
    }

    pub fn get(contains: &str) -> Self {
        Self::any(contains).op(Op::Get)
    }

    pub fn put(contains: &str) -> Self {
        Self::any(contains).op(Op::Put)
    }

    pub fn delete(contains: &str) -> Self {
        Self::any(contains).op(Op::Delete)
    }

    pub fn op(mut self, op: Op) -> Self {
        self.op = Some(op);
        self
    }

    /// Let this many matching requests through before firing: "during shard 3
    /// of 5" is `skip(2)`.
    pub fn skip(mut self, requests: usize) -> Self {
        self.skip = requests;
        self
    }

    /// Fire on this many matching requests, then stop. Defaults to one.
    pub fn times(mut self, requests: usize) -> Self {
        self.times = requests;
        self
    }

    /// Fire on every matching request.
    pub fn always(self) -> Self {
        self.times(usize::MAX)
    }

    fn matches(&self, op: Op, key: &str) -> bool {
        self.op.is_none_or(|wanted| wanted == op) && key.contains(&self.contains)
    }
}

/// What a rule does when it fires.
#[derive(Clone)]
enum Action {
    /// Fail instead of performing the request.
    Fail(Fault),
    /// Perform the request and then fail: the lost acknowledgement that real
    /// networks produce, and the case a naive retry turns into a double write.
    LoseResponse(Fault),
    /// Run a callback just before the request, so one test can interleave two
    /// processes at an exact point.
    Run(Arc<dyn Fn() + Send + Sync>),
}

struct Rule {
    when: When,
    action: Action,
    seen: usize,
    fired: usize,
}

impl Rule {
    /// Consumes one matching request, reporting whether the rule fires on it.
    fn take(&mut self, op: Op, key: &str) -> Option<Action> {
        if !self.when.matches(op, key) || self.fired >= self.when.times {
            return None;
        }
        if self.seen < self.when.skip {
            self.seen += 1;
            return None;
        }
        self.fired += 1;
        Some(self.action.clone())
    }
}

#[derive(Debug, Clone)]
struct Stored {
    body: Vec<u8>,
    content_type: String,
    generation: u64,
    updated_ms: i64,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    faults: Vec<Fault>,
    rules: Vec<Rule>,
    counts: BTreeMap<(Op, String), usize>,
    next_generation: u64,
    clock_ms: i64,
    requests: usize,
}

pub struct MemoryStore {
    state: Mutex<State>,
    latency: Duration,
}

/// Hand-written because a rule holds a callback, which has no `Debug`.
impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap();
        formatter
            .debug_struct("MemoryStore")
            .field("objects", &state.objects.keys().collect::<Vec<_>>())
            .field("requests", &state.requests)
            .finish()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                next_generation: 1,
                clock_ms: 1_700_000_000_000,
                ..State::default()
            }),
            latency: Duration::ZERO,
        }
    }

    /// Delay every operation, for tests that care about elapsed time rather than call count.
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// Queue an error. Faults are consumed in the order they were queued, one per operation, so a
    /// test can spell out "503, 503, then succeed".
    pub fn fail_next(&self, fault: Fault) {
        self.state.lock().unwrap().faults.push(fault);
    }

    /// Fail the requests `when` selects, without performing them.
    pub fn fail_on(&self, when: When, fault: Fault) {
        self.push_rule(when, Action::Fail(fault));
    }

    /// Perform the requests `when` selects and then fail, modelling a write
    /// that lands while its caller is told it did not.
    pub fn lose_response_on(&self, when: When, fault: Fault) {
        self.push_rule(when, Action::LoseResponse(fault));
    }

    /// Run `hook` just before the requests `when` selects, for interleaving a
    /// second writer at an exact point. The hook may use this same store.
    pub fn before(&self, when: When, hook: impl Fn() + Send + Sync + 'static) {
        self.push_rule(when, Action::Run(Arc::new(hook)));
    }

    fn push_rule(&self, when: When, action: Action) {
        self.state.lock().unwrap().rules.push(Rule {
            when,
            action,
            seen: 0,
            fired: 0,
        });
    }

    /// How many requests of that kind touched keys containing `contains`, so a
    /// test can assert that a run read no shards at all.
    pub fn op_count(&self, op: Op, contains: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .counts
            .iter()
            .filter(|((kind, key), _)| *kind == op && key.contains(contains))
            .map(|(_, count)| count)
            .sum()
    }

    /// Forget the request counts, so a test can measure one run out of several.
    pub fn reset_counts(&self) {
        let mut state = self.state.lock().unwrap();
        state.counts.clear();
        state.requests = 0;
    }

    /// Every key currently stored, for whole-bucket assertions.
    pub fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().objects.keys().cloned().collect()
    }

    /// A copy of the stored objects, leaving out keys containing any of
    /// `without`, and carrying over no faults, rules or counts.
    ///
    /// For recomputing derived state from scratch: fork without the rollups,
    /// rebuild them there, and compare with the bucket that was built up
    /// incrementally.
    pub fn fork(&self, without: &[&str]) -> Self {
        let source = self.state.lock().unwrap();
        let objects: BTreeMap<String, Stored> = source
            .objects
            .iter()
            .filter(|(key, _)| !without.iter().any(|skip| key.contains(skip)))
            .map(|(key, stored)| (key.clone(), stored.clone()))
            .collect();
        Self {
            state: Mutex::new(State {
                objects,
                next_generation: source.next_generation,
                clock_ms: source.clock_ms,
                ..State::default()
            }),
            latency: Duration::ZERO,
        }
    }

    /// The stored bytes, bypassing faults and rules.
    pub fn body(&self, key: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(key)
            .map(|stored| stored.body.clone())
    }

    /// How many operations have been attempted, including the failed ones. Retry tests assert on
    /// this rather than on timing.
    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    /// Write without going through the trait, for arranging a test's starting state.
    pub fn seed(&self, key: &Key, body: &[u8]) -> ObjectMeta {
        let mut state = self.state.lock().unwrap();
        Self::write(&mut state, key.path(), body, "application/json")
    }

    /// The store's own clock, for tests that compare it against a caller's idea
    /// of now — clock skew checks, for one.
    pub fn now_ms(&self) -> i64 {
        self.state.lock().unwrap().clock_ms
    }

    /// Move the store's clock forward, for tests about how old an object is
    /// rather than how many writes ago it was written.
    pub fn advance_clock_ms(&self, delta: i64) {
        self.state.lock().unwrap().clock_ms += delta;
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.state.lock().unwrap().objects.contains_key(key.path())
    }

    /// Accounts for a request and applies whatever happens before it: the
    /// keyed rules, then the queued FIFO faults. Returns the lost-response
    /// fault, if any, for the caller to raise once the request has been done.
    fn begin(&self, op: Op, key: &str) -> Result<Option<Fault>> {
        if !self.latency.is_zero() {
            std::thread::sleep(self.latency);
        }

        let mut hooks = Vec::new();
        let mut lost = None;
        let queued = {
            let mut state = self.state.lock().unwrap();
            state.requests += 1;
            *state.counts.entry((op, key.to_string())).or_default() += 1;

            let mut fired = Vec::new();
            for rule in &mut state.rules {
                if let Some(action) = rule.take(op, key) {
                    fired.push(action);
                }
            }
            for action in fired {
                match action {
                    Action::Fail(fault) => return Err(error_for(fault, key)),
                    Action::LoseResponse(fault) => lost = Some(fault),
                    Action::Run(hook) => hooks.push(hook),
                }
            }

            if state.faults.is_empty() {
                None
            } else {
                Some(state.faults.remove(0))
            }
        };

        // Outside the lock: a hook is another process's turn, and it reaches
        // back into this same store.
        for hook in hooks {
            hook();
        }

        match queued {
            Some(fault) => Err(error_for(fault, key)),
            None => Ok(lost),
        }
    }

    fn write(state: &mut State, key: &str, body: &[u8], content_type: &str) -> ObjectMeta {
        let generation = state.next_generation;
        state.next_generation += 1;
        state.clock_ms += 1;
        let stored = Stored {
            body: body.to_vec(),
            content_type: content_type.to_string(),
            generation,
            updated_ms: state.clock_ms,
        };
        let meta = meta_of(key, &stored);
        state.objects.insert(key.to_string(), stored);
        meta
    }
}

fn error_for(fault: Fault, key: &str) -> ObjectStoreError {
    match fault {
        Fault::RateLimited { retry_after_ms } => ObjectStoreError::RateLimited { retry_after_ms },
        Fault::Server { status } => ObjectStoreError::Server {
            status,
            detail: "injected".to_string(),
        },
        Fault::Network => ObjectStoreError::Network {
            detail: "injected".to_string(),
        },
        Fault::Conflict => ObjectStoreError::Conflict {
            key: key.to_string(),
        },
        Fault::Unauthenticated => ObjectStoreError::Unauthenticated {
            source: "injected".to_string(),
            detail: "token expired".to_string(),
        },
    }
}

fn meta_of(key: &str, stored: &Stored) -> ObjectMeta {
    ObjectMeta {
        key: key.to_string(),
        generation: Some(stored.generation.to_string()),
        etag: Some(format!("\"{}\"", stored.generation)),
        size: stored.body.len() as u64,
        updated_ms: Some(stored.updated_ms),
    }
}

fn check(precondition: &Precondition, key: &str, existing: Option<&Stored>) -> Result<()> {
    match precondition {
        Precondition::None => Ok(()),
        Precondition::IfAbsent if existing.is_none() => Ok(()),
        Precondition::IfGenerationMatch(expected)
            if existing.is_some_and(|s| s.generation.to_string() == *expected) =>
        {
            Ok(())
        }
        _ => Err(ObjectStoreError::Conflict {
            key: key.to_string(),
        }),
    }
}

impl ObjectStore for MemoryStore {
    fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, ObjectMeta)>> {
        let lost = self.begin(Op::Get, key.path())?;
        let state = self.state.lock().unwrap();
        let found = state
            .objects
            .get(key.path())
            .map(|stored| (stored.body.clone(), meta_of(key.path(), stored)));
        drop(state);
        match lost {
            Some(fault) => Err(error_for(fault, key.path())),
            None => Ok(found),
        }
    }

    fn put(
        &self,
        key: &Key,
        body: &[u8],
        content_type: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta> {
        let lost = self.begin(Op::Put, key.path())?;
        let mut state = self.state.lock().unwrap();
        check(precondition, key.path(), state.objects.get(key.path()))?;
        let meta = Self::write(&mut state, key.path(), body, content_type);
        drop(state);
        match lost {
            Some(fault) => Err(error_for(fault, key.path())),
            None => Ok(meta),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let lost = self.begin(Op::List, prefix)?;
        let state = self.state.lock().unwrap();
        let found = state
            .objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, stored)| meta_of(key, stored))
            .collect();
        drop(state);
        match lost {
            Some(fault) => Err(error_for(fault, prefix)),
            None => Ok(found),
        }
    }

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()> {
        let lost = self.begin(Op::Delete, key.path())?;
        let mut state = self.state.lock().unwrap();
        let existing = state.objects.get(key.path());
        if existing.is_none() {
            return Err(ObjectStoreError::NotFound {
                key: key.path().to_string(),
            });
        }
        check(precondition, key.path(), existing)?;
        state.objects.remove(key.path());
        drop(state);
        match lost {
            Some(fault) => Err(error_for(fault, key.path())),
            None => Ok(()),
        }
    }
}

/// The content type a seeded or written object was stored with.
impl MemoryStore {
    pub fn content_type(&self, key: &Key) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(key.path())
            .map(|stored| stored.content_type.clone())
    }
}
