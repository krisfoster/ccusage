//! The data model for cloud sync: who the usage belongs to, which machine wrote
//! it, and the shards it is written as.
//!
//! Deliberately free of I/O beyond reading the fingerprint files, so the rules are
//! testable without a bucket: the manifest user ID and the configured values
//! arrive as inputs and the caller decides what to persist or upload.
pub mod duplicates;
pub mod finalize;
pub mod fingerprint;
pub mod fold;
pub mod identity;
pub mod rollup;
pub mod salt;
pub mod shard;

pub use duplicates::{KEY_INDEX_SCHEMA, KeyIndex, mark_duplicates};
pub use finalize::{FINALIZE_AFTER_MS, is_finalized};
pub use fingerprint::{FingerprintSources, observe_fingerprint};
pub use fold::{FoldContext, FoldEntry, fold, utc_date_and_bucket};
pub use identity::{
    IdentityError, IdentityInputs, IdentityOrigin, IdentityWarning, MachineId, ResolvedIdentity,
    UserId, hash_identifier, os_entropy, resolve_identity,
};
pub use rollup::{
    Daily, DailyCell, Derived, Models, Period, Periodic, ROLLUP_SCHEMA, ShardRef, Totals, derive,
};
pub use salt::{DEDUPE_ALGORITHM, Salt, SaltError, SaltOrigin};
pub use shard::{
    BUCKETS_PER_DAY, Cell, Dedupe, DedupeKey, ParsedShard, SHARD_SCHEMA, Session, Shard, ShardError,
};
