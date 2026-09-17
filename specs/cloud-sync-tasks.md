# Cloud Sync + Dashboard — Phased Task Breakdown

Version 2 — incorporates every fix from `specs/cloud-sync-tasks-review.md`.

Companion to `specs/cloud-sync-and-dashboard.md` (the design). That document says *what and why*;
this one is the executable work list: discrete tasks, IDs, dependencies, acceptance criteria.

## How to read this

- **ID** — stable reference (`P2-04`). Use it in branch names and commit scopes.
- **Dep** — tasks that must land first. Anything without a dep in the same phase is parallelizable.
- **Size** — S ≤ 2h, M ≈ half a session, L ≈ a full session of my own throughput.
- **Done** — the acceptance criterion. A task is not done until it is true *and* the repo gates
  pass (`just fmt`, `just check`, `just test`, `just typecheck`, `just hawk` where Rust changes).
- Each task is one squash-merged PR unless marked "fold into".
- Repo conventions that apply to every task: Conventional Commits (`commit` skill), TDD for logic
  (`tdd` skill), docs impact checked via the `docs` skill, `pub` only across crate boundaries
  (`just hawk`), no `ureq`/TLS outside the binary crate.

Legend for risk: 🔴 blocking unknown, 🟡 known-hard, ⚪ routine.

---

## Phase 0 — Spikes (resolve blocking unknowns)

No production code ships from this phase. Every task's deliverable is a decision recorded as a new
"Decision Record" section appended to the design doc, plus throwaway probe code under `/tmp`.

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P0-01 | **Credential matrix + binary-size measurement** (S1). Prototype HMAC/SigV4, ADC file + refresh-token grant, and service-account RS256; measure `cargo bloat` delta for each against current `main`. | — | M | 🟡 | Table of size deltas; v1 credential set chosen; RSA in/out decided against the ≤150 KiB budget. |
| P0-02 | **Public-page/private-data probe** (S12 + S5). Real bucket: UBLA on, conditional `allUsers:objectViewer` scoped to `…/objects/<prefix>/dashboard/`, PAP relaxed, private data objects, browser GIS token read of a private object, CORS behavior same-origin vs. custom domain. | — | L | 🔴 | Screenshot/log of an anonymous page load + a signed-in data read + an anonymous 403 on data. Decision: mode A default, or fall back to B/C. Confirms PAP can be relaxed and re-enforced. |
| P0-03 | **Machine fingerprint survey** (S3). `/etc/machine-id`, `IOPlatformUUID`, `MachineGuid`, containers, WSL, devcontainers. | — | M | 🟡 | Chosen source per OS + collision mitigation + override story. |
| P0-04 | **Provider identity sources** (S11). Shape/stability of `~/.claude.json.oauthAccount`; equivalents for Codex et al.; behavior for API-key / Bedrock / Vertex / multi-account users. | — | M | 🟡 | The §4.0 resolution chain confirmed or revised; a fallback that never blocks sync. |
| P0-05 | **Google OAuth client viability** (S9). Installed-app + PKCE, sensitive-scope verification requirements and caps. | — | S | 🔴 | Go/no-go for the "no gcloud" rung; if no-go, ladder rung 3 is cut from v1. |
| P0-06 | **Object-count / cost / latency envelope** (S2). Simulate 2 machines × 18 months under the §5.3 layout. | — | S | ⚪ | Object count, monthly GCS cost, cold-load GET count vs. the ≤3-GET target. |
| P0-07 | **Dashboard bundle budget** (S6). Minimal Vite+TS+uPlot build, gzipped size. | — | S | ⚪ | Number vs. the 200 KiB budget; embed-vs-download decided. |
| P0-08 | **Project/billing onboarding UX** (S10) and **privacy defaults** (S8), incl. the salt design of P3-14. | — | S | ⚪ | Guided no-project path written out; redaction defaults fixed; what-leaves-the-machine table drafted. |
| P0-10 | **Model equivalence + price redistribution** (S7). Curate the tier map; confirm whether models.dev / LiteLLM licences permit copying prices into a world-readable bucket object, and what attribution is required. | — | M | 🟡 | Draft `model-equivalence.json`; a written licence answer (this gates P4-05, which publishes the data). |
| P0-11 | **Shared log-tree prevalence** (S4). How often is one `~/.claude/projects` visible to two machines (Dropbox/iCloud/NFS/devcontainer bind mounts)? | — | S | ⚪ | Decides whether P3-07 is a must-have or a thin safety net, and whether detection-at-setup (warn on network volumes) is the better fix. |
| P0-09 | **Fold decisions into the design doc**; open follow-up issues for anything descoped. | P0-01…08, P0-10, P0-11 | S | ⚪ | Design doc has a Decision Record section; task list below amended where a spike changed it. Includes re-reviewing A1/C1 from the review doc. |

**Status.** P0-03, P0-04, P0-05, P0-06, P0-07 and P0-10 are answered — see §9 "Decision Record"
of the design doc (DR-03…DR-10). Two of them changed the design rather than confirming it: DR-04
replaces provider-derived identity with a bucket-derived user ID (so no credential file is ever
read), and DR-05 cuts the built-in OAuth client from v1, which shrinks P2-04 and removes the
browser-sign-in dependency from the default share path. P0-02 remains blocked on a billing-enabled
GCP project; P0-01, P0-08 and P0-11 are not yet run and gate nothing in Phase 1.

Exit criteria for the phase: P0-01, P0-02, P0-03, P0-04, P0-10 answered. P0-05/06/07/08/11 may
trail into Phase 1 if they only affect later phases — but P0-06 must land before P3-01 and P0-05
before P2-04.

---

## Phase 1 — Object store abstraction + GCS client

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P1-01 | Create `rust/crates/ccusage-objectstore`: `ObjectStore` trait, `ObjectMeta`, `Precondition`, `ObjectStoreError` taxonomy (`Conflict`, `NotFound`, `Forbidden`, `Unauthenticated`, `Network`, `Server`), key builder. No HTTP deps. Crate README stating the Crane layer. | P0-01 | M | ⚪ | Crate builds, `just hawk` clean, unit tests for key building and error mapping. |
| P1-02 | `MemoryStore` test double in `ccusage-test-support`: CAS semantics, injectable 412/429/5xx, latency. | P1-01 | S | ⚪ | Every later phase can test sync logic with no network. |
| P1-03 | SigV4/`GOOG4-HMAC-SHA256` signer (pure `hmac`+`sha2`, no TLS). | P0-01 | M | 🟡 | Canonical-request and string-to-sign unit tests against Google's documented vectors. |
| P1-04 | `GcsStore` in the binary crate next to `http.rs`: `get`/`put`/`list`/`delete`, `ifGenerationMatch`/`ifAbsent`, retry with jitter for 429/5xx, no retry for 412. | P1-01, P1-03 | L | 🟡 | Mocked-HTTP tests for each verb + precondition; `ureq` still absent from every non-binary crate's lockfile graph. |
| P1-05 | Credential providers: env, file, ADC (file + `gcloud` fallback + metadata server), HMAC; token cache with expiry. | P0-01 | L | 🟡 | Resolution order tested; errors name the source that was tried. |
| P1-06 | Bucket admin ops: `get_bucket`, `create_bucket` (UBLA, PAP, location, soft-delete, lifecycle), `patch_bucket` (**PAP enforce ⇄ inherited**, website config), `set_iam_policy` (conditional binding), `set_cors`, and IAM `signBlob` (needed for Mode B signing under ADC, where no HMAC secret exists). | P1-04 | M | 🟡 | Unit-tested against mocked responses incl. 409/403 paths and an org-policy-pinned PAP failure. |
| P1-07 | Optional keychain storage behind a cargo feature; size-checked. | P1-05 | S | ⚪ | Off by default if it costs more than the budget allows. |

**Status.** P1-01…P1-04 have landed on `dashboard`: the `ccusage-objectstore` crate (keys, error
taxonomy, `ObjectStore`, V4 signer pinned to the published `get-vanilla` vector), the `MemoryStore`
double in `ccusage-test-support`, and `GcsStore` in the binary crate — JSON API get/put/list/delete
over the existing `ureq` seam, `ifGenerationMatch` preconditions (`0` for create-if-absent), and
backoff on 429/5xx/network only, with 412 surfaced as `Conflict` on the first attempt so a CAS loop
re-reads instead of overwriting. P1-05 followed: the credential ladder is env access token → env
HMAC pair → `GOOGLE_APPLICATION_CREDENTIALS` → the well-known ADC file → `gcloud auth
print-access-token` → metadata server, with a token cache that refreshes 60s before expiry and an
exhaustion error that names every rung it tried. Service-account and external-account files are
rejected with remediation rather than pulling in an RSA signer (DR-05). Both are tested against a
scripted loopback server in `ccusage-test-support`, so the assertions are on the bytes that go out.
P1-06 then landed `gcs::bucket`: an idempotent `ensure` (get, create, and a 409 re-read so a second
machine adopts the winner's bucket), uniform bucket-level access forced on at creation, soft delete
off, a `publicAccessPrevention` toggle, CORS limited to GET/HEAD, and a conditional `allUsers`
object-viewer binding whose expression is built from `KeySpace::public_prefix` so the grant cannot
reach past the dashboard prefix. `signBlob` goes through the IAM Credentials API, so share links can
be minted without a private key on the machine. The request plumbing is now a shared `JsonApi` —
`GcsStore` and `BucketAdmin` use the same authorization, status mapping and backoff. Phase 1 is
complete; Phase 2 (`sync setup`) is next, and P0-02 still needs a billing-enabled project.

---

## Phase 2 — `ccusage sync setup` / `status` / `doctor`

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P2-01 | `SyncArgs`/`SyncCommand` types in `ccusage-cli`; add `sync` to `is_command()`; parser + subcommand dispatch; embedded help JSON entries. Defines the **full** subcommand grammar up front — `setup`, `status`, `doctor`, `run`, `repair`, `forget`, `merge-machine`, `dashboard` — because help JSON and snapshots regenerate once. | — | M | ⚪ | Parser tests cover every flag; subcommands whose behavior lands in a later phase exit with "available in a later release", not a parse error; bad-flag messages match existing style. |
| P2-02 | `sync` config block in `ccusage-config` + regenerate `apps/ccusage/config-schema.json` + update config snapshots. | P2-01 | M | ⚪ | Config→args precedence tested; schema snapshot test green. |
| P2-03 | Identity resolution (`userId`, `machineId`) per design §4.0 and DR-03/DR-04, in a new `ccusage-sync` crate. Provider accounts are **not** an input — DR-04 dropped `~/.claude.json` as an identity source — so the user ID comes from config, else the bucket manifest, else a minted ID, and the hardware fingerprint only detects a copied config. | P0-03, P0-04 | M | 🟡 | Fixture-backed tests incl. an absent fingerprint source, a fingerprint that changed under a configured machine ID, and override precedence. |
| P2-04 | Credential ladder wiring + interactive prompts (`--auth auto`), incl. optionally invoking `gcloud auth application-default login`. | P1-05, P2-01, P0-05 | L | 🟡 | Each rung selectable and testable; `--non-interactive` never prompts. Any `gcloud` invocation is explicit (never silent), uses an absolute resolved path, spawns without a shell, and its output is never logged. Rung 3 is cut cleanly if P0-05 said no-go. |
| P2-05 | Project discovery/selection + the no-project guided message. | P1-05 | M | ⚪ | One project → auto; many → picker; none → exact instructions. |
| P2-06 | Idempotent bucket resolution/creation per §4.1.2, writing `sync.bucket`/`sync.projectId` back to config. Creates with `publicAccessPrevention: inherited` per DR-11 — `enforced` would refuse the conditional dashboard binding P5-08 adds, so containment comes from the prefix condition instead, and nothing may widen the binding beyond `KeySpace::public_prefix`. | P1-06, P2-02, P2-05 | L | 🟡 | Reuse-existing, create-new, 409-retry, 403-explain all tested; refuses to create a second bucket without `--recreate`; a freshly created bucket is verifiably not publicly readable. |
| P2-07 | `sync status` (resolved identity/target/credential source/last sync) + `--json`. | P2-03, P2-04 | M | ⚪ | Table + JSON snapshots; never prints a secret. |
| P2-08 | `sync doctor`: credential check, bucket reachability, write+CAS probe on `<prefix>/.probe/<machineId>` (deleted in a guard, never under the public prefix), clock-skew check vs. `Date` header, IAM/PAP diagnostics, and a warning when the agent log root looks like a network/synced volume (the P3-07 trigger). | P2-06 | M | ⚪ | Each check has a pass/fail/remediation line; exit codes documented; the probe object is gone even when the process is killed mid-check. |
| P2-09 | Secret hygiene: gitleaks rule for HMAC ids, permission warnings on credential files, redaction in all output/logs. Written as a **reusable assertion helper**, since P1-07, P4-05 and P5-06 all add sensitive material later. | P2-04 | S | ⚪ | Test asserts no secret — HMAC secret, bearer token, signed-URL signature — appears in any rendered output or log line. |
| P2-10 | Docs: `docs/guide/cloud-sync.md` (setup only) + VitePress nav + environment-variables guide entries. | P2-07, P2-08 | M | ⚪ | Doc builds; `docs` skill checklist walked. |

---

## Phase 3 — Shard model + sync engine

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P3-01 | Shard data model + (de)serialization: 15-min UTC buckets, per-model rows, **per-(bucket, model) dedupe key sets** (not per-shard — see review A1), session index, `contentHash`, `schema` version. | P2-03, P0-06 | L | 🟡 | Round-trip and golden-file tests; hash is stable across runs and machines; serialized size for a heavy day stays within the P0-06 envelope. |
| P3-02 | Folding local adapter entries → shards (tokens authoritative, cost recomputed, project redaction). | P3-01 | L | 🟡 | Fixture-backed tests incl. sessions crossing midnight/DST and the 15-min boundary. |
| P3-03 | Per-machine `index.json` + `machine.json` (single-writer) read/write with self-CAS. | P3-01, P1-02 | M | ⚪ | Diffing by `contentHash` skips unchanged shards. |
| P3-04 | `manifest.json` multi-writer CAS update with bounded jittered retry. | P1-02, P3-03 | M | 🟡 | Concurrent-writer test with injected 412s converges. |
| P3-05 | Finalization rules (48h) + anomaly recording for late edits to finalized shards. | P3-03 | S | ⚪ | Warning surfaced in output and stored in the shard/rollup. |
| P3-13 | Schema compatibility on the Rust side: a shard written by a **newer** ccusage (the other machine auto-updated first) is read as opaque — counted, excluded from rollups, reported as "N shards need ccusage ≥ X" — never parsed leniently and undercounted. | P3-01 | M | 🟡 | Test with a synthetic `schema: 99` shard: no error, no silent undercount, clear message. |
| P3-14 | Per-bucket hash salt: generated at setup, stored in the private prefix and mirrored in config, applied to `userId`, project paths and dedupe keys. Missing/mismatched salt is a hard stop, not a silent unsalted fallback. | P2-06, P3-01 | M | 🟡 | Test proves identical project paths hash differently across two buckets; `sha256(path)` without salt appears nowhere. |
| P3-06 | Rollup computation (`daily`/`weekly`/`monthly`/`models`) with incremental `basedOn` hash map + CAS write. Suppression hooks (P3-07) are designed in from the start rather than retrofitted. | P3-03, P3-04 | L | 🟡 | Rollup equals a full recompute in property tests; only changed shards are fetched. |
| P3-07 | Cross-machine duplicate handling per §5.5: intersect per-(bucket, model) key sets; **total** overlap ⇒ suppress the cell on the larger `machineId` and record `duplicateSuppressed`; **partial** overlap ⇒ suppress nothing, record `duplicateSuspected` + ratio. | P3-01, P3-06, P0-11 | M | 🟡 | Tests for all three cases (disjoint / total / partial). Suppression is exact or declines to act — it never removes tokens it cannot prove are duplicates. |
| P3-08 | `ccusage sync` command: local lock file, pipeline orchestration, progress reporting consistent with existing progress/spinner conventions, `--dry-run`, `--since/--until`, `--agent`, `--force`, `--offline` guard mirroring the pricing flag. | P3-02…07, P2-06 | L | 🟡 | End-to-end test against `MemoryStore`; second concurrent local run is refused cleanly; `--offline` never opens a socket. |
| P3-09 | `sync repair` (rebuild manifest/rollups by listing), `sync forget <machine>` / `--prune` retention, and `sync merge-machine <old> <new>` (§5.5 — the OS-reinstall case). | P3-06 | M | ⚪ | Repair reconstructs byte-identical rollups after deliberate corruption; `merge-machine` is explicit, reversible-by-repair, and refuses to merge two machines with overlapping live shards. |
| P3-10 | Failure-path hardening: crash between shard PUT and index PUT, expired creds mid-run, offline, skewed clock. | P3-08 | M | 🟡 | Each simulated in tests; no state loss, exit codes documented. |
| P3-11 | Optional `--pull` cache + `ccusage daily --from-cloud` (drop if it slips; it is the one open scope question in the design). | P3-06 | L | ⚪ | Explicit go/no-go decision recorded before starting. |
| P3-12 | Docs: sync half of `docs/guide/cloud-sync.md` — merge semantics, machine model, privacy, failure modes. | P3-08 | M | ⚪ | Includes the "what leaves my machine" table. |

---

## Phase 4 — `ccusage compare` (provider counterfactual, headless)

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P4-01 | `model-equivalence.json` (curated, dated, versioned) + loader + override hook. | P0-10 | M | 🟡 | Covers every model appearing in the repo's pricing fixtures; unmapped models are reported, never silently dropped. |
| P4-02 | Counterfactual cost computation over `PricingMap`, incl. cache-write/cache-read fallback rules. | P4-01 | M | 🟡 | Unit tests per rule; z.ai vs. Anthropic worked example matches hand-computed numbers. |
| P4-03 | `ccusage compare` command: parser, args, table output, `--json`, `--provider`, `--mode` interaction. | P4-02, P2-01 | M | ⚪ | CLI snapshot + JSON snapshot tests. |
| P4-04 | Subscription/plan rows (Claude Max, GLM Coding Plan) presented separately from per-token math. | P4-03 | S | 🟡 | Plan data is config-driven, dated, and labelled as such. |
| P4-05 | Pricing emission so the browser needs no third-party fetch, incl. attribution line. Deliberately classified **public**: it is third-party price data, not user data, so it lives under `<prefix>/dashboard/pricing.json` and the page renders provider tables before sign-in. This classification is explicit, reviewed, and enforced by P5-11 — not an ad-hoc placement. | P4-02, P0-10 | S | 🟡 | Licence answer from P0-10 quoted in the PR description; attribution rendered in the UI footer; no user-derived field present in the object (asserted by test). |
| P4-06 | Docs: `docs/guide/provider-comparison.md` with the caveats section (tokenizer non-portability, cache-pricing differences). | P4-03 | M | ⚪ | Caveats appear in docs *and* in CLI output footer. |

---

## Phase 5 — Dashboard app + access split

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P5-01 | `apps/ccusage-dashboard` workspace package: Vite + TS, justfile module, lint/typecheck wiring, catalog-pinned deps. | P0-07 | M | ⚪ | `just build` / `just typecheck` include it; supply-chain settings respected. |
| P5-02 | Data layer: fetch manifest + rollups, timezone re-bucketing from 15-min buckets, schema-version migration on read. | P5-01, P3-06 | L | 🟡 | Unit tests incl. `+05:45` offsets and DST transitions. |
| P5-03 | Core views: totals header, time series, model table, machine/project panels, freshness + anomalies strip. | P5-02 | L | ⚪ | Renders from fixture data in a headless test; empty/partial states handled. |
| P5-04 | Provider comparison table in the UI, sharing P4 semantics, with per-row equivalence override. | P5-03, P4-05 | M | 🟡 | Numbers match `ccusage compare --json` for the same fixture — asserted in a test. |
| P5-05 | Access mode A: browser Google sign-in (PKCE), in-memory token only (never `localStorage` — every public bucket shares the `storage.googleapis.com` origin), anonymous empty state. | P0-02, P5-02, P5-08 | L | 🔴 | Anonymous load works and shows sign-in; signed-in load renders data; 403 renders a clear message; no token is reachable from another page on the shared origin. |
| P5-06 | Access mode B: `sync dashboard --share [--ttl]` signed-URL link with fragment-carried URLs, plus `--revoke`. | P1-03, P1-06, P5-02 | M | 🟡 | Link works, expires, and never puts URLs in the query string. Default TTL in hours, expiry shown in the UI, and `--revoke` (rotate HMAC key / bump prefix salt) actually invalidates outstanding links — a signed URL is a bearer token and is otherwise unrecallable once it reaches browser history or a chat preview. |
| P5-07 | Access mode C: client-side AES-GCM encryption of data objects + passphrase unlock (only if P0-02 demands it). | P5-02 | L | 🟡 | Round-trip CLI-encrypt → browser-decrypt test; documented no-recovery warning. |
| P5-08 | `sync dashboard` deploy: upload bundle with content-hashed asset names, `Content-Type`/`Cache-Control`, **PAP enforced → inherited**, IAM conditional binding scoped to the dashboard prefix, CORS for custom domains, `--i-understand-public` gate printing exactly what becomes public. | P1-06, P5-01, P5-09 | L | 🟡 | Re-deploy is idempotent; downgrade re-enforces PAP and removes the binding; an org policy pinning PAP produces a clear message pointing at modes B/C rather than a raw 403. |
| P5-09 | Bundle embedding in the Rust binary + size-budget test (or the release-download fallback), with a `--bundle-dir` dev escape so P5-08 is testable before embedding is finished. | P0-07, P5-01 | M | 🟡 | Test fails if the compressed bundle exceeds budget. |
| P5-10 | Docs: `docs/guide/dashboard.md` with screenshots, the access-model table, and the custom-domain recommendation. | P5-05, P5-08 | M | ⚪ | Screenshots in `docs/public/`, guide leads with the primary one per docs conventions. |
| P5-11 | **Key-space invariant, enforced in code**: every object is classified `Public` or `Private` at the type level, and the store wrapper refuses to write a `Private` object under `<prefix>/dashboard/` (and vice versa). | P5-08 | M | 🔴 | A test attempts to write a rollup under the public prefix and fails to compile or panics in debug + errors in release. This single invariant is what stands between the design and publishing the user's spend data. |

---

## Phase 6 — Release readiness

| ID | Task | Dep | Size | Risk | Done |
|---|---|---|---|---|---|
| P6-01 | End-to-end test against a real GCS bucket, gated behind an env var so CI skips it by default. | P3-08, P5-08 | L | 🟡 | Two simulated machines → one merged dashboard, verified assertions. |
| P6-02 | Performance check (`profile` skill): `sync` on a large fixture; assert no regression in existing report commands. | P3-08 | M | ⚪ | Branch-vs-main numbers recorded in the PR. |
| P6-03 | Binary-size review (`rust-binary-size`): final `cargo bloat` vs. pre-feature baseline. | P5-09 | M | 🟡 | Total delta within the agreed budget, or an explicit waiver. |
| P6-04 | Security review pass: re-run the P2-09 hygiene assertions across everything added since, verify public-prefix scope and the P5-11 invariant, signed-URL TTL/revocation, redaction and salt defaults. Write the **threat model** into the design doc, including read-access blast radius — object *names* alone leak machine ids, agents used and an exact activity calendar, so granting a teammate `objectViewer` grants all of that; recommend Mode B links over IAM grants for sharing. | P5-08, P5-11 | M | 🟡 | Checklist completed in the PR description; threat model section merged. |
| P6-05 | README + docs index + `all-reports`/`json-output` cross-links; release notes copy. | P5-10, P4-06 | M | ⚪ | `docs` skill checklist walked for every entrypoint listing commands. |

---

## Sequencing summary

```
P0 (spikes)
 └─> P1 (objectstore + GCS)
      └─> P2 (setup/status/doctor)
           └─> P3 (shards + sync engine)
                └─> P5 (dashboard + access)   P5-08 ← P5-09;  P5-05 ← P5-08;  P5-11 gates release
                     └─> P6 (release readiness)

P4 (compare) hangs off P2-01 + P4-01/02 only — it is pure computation and can run alongside P3.
```

Parallelizable once P1 lands: P4-01/P4-02 (pure computation, no storage dependency) and P5-01
(package scaffolding) can run alongside P2/P3.

## Estimated effort

Summing the task sizes (S ≈ 0.15, M ≈ 0.5, L ≈ 1.0 sessions) gives **15–18 sessions** of my own
throughput end to end — the v1 estimate of 9–12 was understated. Phase 5 alone is ~4, Phase 3 ~4.5.
Phase 0 gates a large fraction of the rest and can *remove* work as easily as add it: a no-go on
S9 or S12 deletes tasks outright. The external waits — Google OAuth verification (P0-05) if we need
the no-gcloud rung, and any org-policy exceptions for public prefixes — are calendar time, not work
time, and should be started on day one.

## Cut lines (if scope needs trimming)

1. P3-11 (`--from-cloud` reports) — pure addition, cut first.
2. P5-07 (client-side encryption) — only needed if P0-02 fails.
3. P4-04 (subscription rows) — the per-token comparison stands alone.
4. Ladder rung 3 (built-in OAuth) — ADC + HMAC cover most users; cut if P0-05 is a no-go.
5. P3-07 (cross-machine duplicate handling) — cut to detection-and-warn only if P0-11 shows shared
   log trees are rare.

Not cuttable, whatever the schedule looks like: **P5-11** (key-space invariant), **P3-14** (hash
salt) and **P3-13** (schema compatibility). Each is cheap now and either a data leak or a silent
undercount if deferred.
