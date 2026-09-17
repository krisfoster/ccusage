# ccusage-test-support

Test-only helpers: filesystem fixtures and environment-variable guards.

## Owns

- `Fixture` and the `fs_fixture!` macro — build a temporary directory tree from a
  literal file map, so loader tests read real files instead of mocks.
- `EnvVarGuard` and `EnvVarsGuard` — set the data-directory variables an adapter
  reads and restore them when the test ends.
- `zcode::create_fixture` — create the representative ZCode SQLite schema and
  usage rows shared by adapter and unified report tests.
- `objectstore::MemoryStore` and `objectstore::Fault` — an in-memory `ObjectStore`
  with GCS-shaped generations, so CAS loops and retry policy are testable without
  a bucket, and queued faults put a 412, 429, 5xx or dropped connection exactly
  where a test wants one.
- `http_server::ScriptedServer` — a loopback HTTP server that replays a list of
  responses and records the raw requests, so the GCS and credential clients are
  tested over a real socket and a test can assert on the bytes that went out.

Every crate that has tests uses this as a dev-dependency; nothing depends on it
at runtime.

## Public surface

- `EnvVarGuard`
- `EnvVarsGuard`
- `Fixture`
- `http_server::ScriptedServer`
- `http_server::json_response`
- `http_server::response`
- `objectstore::Fault`
- `objectstore::MemoryStore`
- `zcode::create_fixture`

## Depends on

- `assert_fs`
- `ccusage-objectstore`
- `jiff`
- `sqlite`

## Build layer

Built in the `foundation` Crane artifact layer, so a change here recompiles every adapter. It is a dev-dependency only, so it never reaches the shipped binary.
