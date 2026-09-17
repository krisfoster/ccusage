# ccusage-objectstore

The provider-neutral half of cloud sync: the `ObjectStore` trait, the key layout, the error
taxonomy, and V4 request signing. No HTTP client and no TLS — the sync engine can depend on this
without pulling a network stack into every crate that builds above it, and the signer is testable
against published vectors without a socket. The GCS implementation lives in `rust/crates/ccusage`
next to `http.rs`, the same seam pricing uses.

## Owns

- `key.rs` — `KeySpace`, the single place object keys are built, plus `UtcDate` and the
  `Visibility` classification.
- `error.rs` — `ObjectStoreError` and the HTTP status mapping.
- `store.rs` — the `ObjectStore` trait, `ObjectMeta`, and `Precondition`.
- `sign.rs` — `GOOG4-HMAC-SHA256` and `AWS4-HMAC-SHA256` signing. One implementation with the
  provider literals as a `Scheme` parameter, because the two differ only in those literals; the
  tests pin it to the published `aws-sig-v4-test-suite` `get-vanilla` vector.

## Two invariants enforced here rather than by convention

- **Untrusted path segments.** `userId`, `machineId` and `agent` reach the key builder from config
  and from discovered provider identity, so they are validated as single path segments; anything
  that could climb out of one is rejected.
- **The public/private split.** The dashboard prefix is world-readable and everything else is not.
  A `Key` carries its `Visibility`, and `dashboard_asset` is the only constructor that produces a
  public one, so a store implementation cannot write a data object into the public prefix by
  accident. `KeySpace::public_prefix` exposes that same prefix to the bucket's IAM grant, so the
  `allUsers` binding and the keys it is meant to cover cannot drift apart.

## Public surface

- `error::ObjectStoreError`, `error::Result`
- `key::Key`, `key::KeySpace`, `key::RollupKind`, `key::UtcDate`, `key::Visibility`
- `store::ObjectMeta`, `store::ObjectStore`, `store::Precondition`
- `sign::{AWS4_HMAC_SHA256, GOOG4_HMAC_SHA256, CanonicalRequest, HmacKey, Scheme, Signer,
  SigningTime, sha256_hex}`

## Depends on

- `hmac`
- `sha2`

## Build layer

Built in the `foundation` Crane artifact layer.
