# ccusage-sync

Identity for cloud sync: the `userId` every object in a bucket shares and the `machineId` that
separates one writer's shards from another's. No HTTP and no config parsing — the manifest value
and the configured values arrive as inputs, so the resolution rules are testable without a bucket
and the binary crate decides what to persist.

## Owns

- `identity.rs` — `resolve_identity`, the origin of each resolved value (`Configured`, `Manifest`,
  `Minted`), the copied-config warning, and `hash_identifier`, the salted hash that reaches an
  object key.
- `fingerprint.rs` — the hashed machine fingerprint the copied-config warning compares.

## Why neither identifier is derived from the machine

Both decisions are recorded in `specs/cloud-sync-and-dashboard.md`:

- **DR-03.** Hardware IDs collide — a cloned VM or a container image carries one `/etc/machine-id`
  into many machines, and two machines sharing a shard key interleave writes. The machine ID is
  therefore 128 bits of OS entropy minted once, and the fingerprint is demoted to detecting a
  copied config (fingerprint changed, machine ID did not).
- **DR-04.** The user ID is not read from a provider credential file: the field is absent for
  API-key, Bedrock and SSO users, and those files hold live access tokens next to it. The ID comes
  from the bucket's manifest instead, which is the thing the user actually authenticated to.

Fingerprint sources are best-effort by design: macOS and Windows keep theirs behind IOKit and the
registry, so there the fingerprint is absent and the warning simply does not fire.

## Public surface

- `fingerprint::{FingerprintSources, observe_fingerprint}`
- `identity::{IdentityError, IdentityInputs, IdentityOrigin, IdentityWarning, MachineId,
  ResolvedIdentity, UserId, hash_identifier, os_entropy, resolve_identity}`

## Depends on

- `getrandom`
- `sha2`

## Build layer

Built in the `foundation` Crane artifact layer.
