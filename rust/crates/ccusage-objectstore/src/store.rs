//! The storage trait itself.

use crate::error::Result;
use crate::key::Key;

/// What a store knows about an object without reading its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    /// The provider's version token — GCS generation, S3 version id. Carried opaquely so the
    /// same CAS loop works on providers whose tokens are not numbers.
    pub generation: Option<String>,
    pub etag: Option<String>,
    pub size: u64,
    pub updated_ms: Option<i64>,
}

/// The write precondition, i.e. compare-and-swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// Unconditional. Only for objects with a single writer that owns them outright.
    None,
    /// Succeed only if the object does not exist yet.
    IfAbsent,
    /// Succeed only if the object is still at this generation.
    IfGenerationMatch(String),
}

pub trait ObjectStore {
    fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, ObjectMeta)>>;

    fn put(
        &self,
        key: &Key,
        body: &[u8],
        content_type: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta>;

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()>;
}
