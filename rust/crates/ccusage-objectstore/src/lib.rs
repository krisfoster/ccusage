//! Provider-neutral object storage for cloud sync.
//!
//! The trait and its supporting types live here so the sync engine can be written and tested
//! against a fake store; the HTTP implementations live in the binary crate, which is the only
//! place a TLS stack is allowed.

pub mod error;
pub mod key;
pub mod sign;
pub mod store;

pub use error::{ObjectStoreError, Result};
pub use key::{Key, KeySpace, RollupKind, UtcDate, Visibility};
pub use sign::{AWS4_HMAC_SHA256, GOOG4_HMAC_SHA256, HmacKey, Signer, SigningTime, sha256_hex};
pub use store::{ObjectMeta, ObjectStore, Precondition};
