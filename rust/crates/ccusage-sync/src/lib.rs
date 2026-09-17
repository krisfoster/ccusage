//! Identity for cloud sync: who the usage belongs to and which machine wrote it.
//!
//! Deliberately free of I/O beyond reading the fingerprint files, so the resolution
//! rules are testable without a bucket: the manifest user ID and the configured
//! values arrive as inputs and the caller decides what to persist.
pub mod fingerprint;
pub mod identity;

pub use fingerprint::{FingerprintSources, observe_fingerprint};
pub use identity::{
    IdentityError, IdentityInputs, IdentityOrigin, IdentityWarning, MachineId, ResolvedIdentity,
    UserId, hash_identifier, os_entropy, resolve_identity,
};
