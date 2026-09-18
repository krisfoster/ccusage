//! An in-memory `ObjectStore`, so the sync engine's merge and retry logic can be tested without
//! a network or a bucket.
//!
//! Generations behave the way GCS's do — a monotonically increasing token per object, changed by
//! every successful write — because the CAS loops under test are only interesting when a second
//! writer can move the object between the read and the write. `fail_next` is what lets a test put
//! that second writer, or a 503, exactly where it wants it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use ccusage_objectstore::{Key, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, Result};

/// An error to return instead of performing the next operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    RateLimited { retry_after_ms: Option<u64> },
    Server { status: u16 },
    Network,
    Conflict,
}

#[derive(Debug, Clone)]
struct Stored {
    body: Vec<u8>,
    content_type: String,
    generation: u64,
    updated_ms: i64,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    faults: Vec<Fault>,
    next_generation: u64,
    clock_ms: i64,
    requests: usize,
}

#[derive(Debug)]
pub struct MemoryStore {
    state: Mutex<State>,
    latency: Duration,
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

    fn begin(&self, key: &str) -> Result<()> {
        if !self.latency.is_zero() {
            std::thread::sleep(self.latency);
        }
        let mut state = self.state.lock().unwrap();
        state.requests += 1;
        if state.faults.is_empty() {
            return Ok(());
        }
        Err(match state.faults.remove(0) {
            Fault::RateLimited { retry_after_ms } => {
                ObjectStoreError::RateLimited { retry_after_ms }
            }
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
        })
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
        self.begin(key.path())?;
        let state = self.state.lock().unwrap();
        Ok(state
            .objects
            .get(key.path())
            .map(|stored| (stored.body.clone(), meta_of(key.path(), stored))))
    }

    fn put(
        &self,
        key: &Key,
        body: &[u8],
        content_type: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta> {
        self.begin(key.path())?;
        let mut state = self.state.lock().unwrap();
        check(precondition, key.path(), state.objects.get(key.path()))?;
        Ok(Self::write(&mut state, key.path(), body, content_type))
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        self.begin(prefix)?;
        let state = self.state.lock().unwrap();
        Ok(state
            .objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, stored)| meta_of(key, stored))
            .collect())
    }

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()> {
        self.begin(key.path())?;
        let mut state = self.state.lock().unwrap();
        let existing = state.objects.get(key.path());
        if existing.is_none() {
            return Err(ObjectStoreError::NotFound {
                key: key.path().to_string(),
            });
        }
        check(precondition, key.path(), existing)?;
        state.objects.remove(key.path());
        Ok(())
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
