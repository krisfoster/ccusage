# Specification: Cloud Sync + Hosted Dashboard for ccusage

Status: draft design for review (no code written)
Decisions locked with the user: **one bucket per user** (never per machine — the machine is only a
shard key inside the bucket, see §4.0); auth and bucket creation must be as close to zero-friction
as possible; HMAC/SigV4 + ADC for credentials; 15-minute UTC buckets for storage granularity.
Scope: three features — (1) `ccusage sync setup` cloud target configuration, (2) `ccusage sync`
merge/upload, (3) a static dashboard served from the bucket with a cross-provider cost comparison.

---

## 1. Repository context that constrains the design

Findings from reading the tree (these are the load-bearing facts for every decision below):

- **Rust-first.** The production CLI is `rust/crates/ccusage` (dispatch only), with shared logic in
  `ccusage-core` and one crate per source in `rust/adapters/<agent>`. `apps/ccusage` is just the npm
  launcher + `config-schema.json`. New runtime behavior must go in Rust.
- **Commands are hand-parsed.** `rust/crates/ccusage-cli-parser/src/parser.rs` has an explicit
  `is_command()` allowlist, typed arg structs in `ccusage-cli`, and embedded help JSON. A new `sync`
  command is a parser + help + config-schema change, not a derive-macro change.
- **Config is layered JSON.** `ccusage-config` discovers `./.ccusage/ccusage.json` and a user-level
  `ccusage.json`, merging `defaults` → `commands.<name>` → agent sections. `config-schema.json` in
  `apps/ccusage` is a generated artifact with snapshot tests. A `sync` config block fits this shape
  naturally — but **secrets must not live there** (see §4.3).
- **No network stack in core, on purpose.** `ureq` + rustls live only in the binary crate
  (`rust/crates/ccusage/src/http.rs`), injected into core via
  `pricing::set_json_fetcher`. Binary size is an explicit concern (`rust-binary-size` skill). The
  object-store client must follow the same seam: trait in a lib crate, HTTP impl in the binary.
- **Dedup already exists and is per-`(message.id, request_id, session_id)`** — see
  `push_deduped_entry` in `rust/adapters/claude/src/lib.rs`. This is the natural cross-machine
  identity for a usage record, and it means the merge problem is solvable exactly, not heuristically.
- **Pricing is already multi-provider.** `models-dev-pricing.json` embedded in `ccusage-core`
  already contains Anthropic *and* z.ai/GLM entries with `input` / `output` / `cache_read` /
  `cache_write` rates (verified: `TEE/glm-5.3`, `Pro/zai-org/GLM-5.1`, `anthropic--claude-4.5-sonnet`).
  Feature 3's comparison table needs a **model equivalence map**, not a new price source.
- **Reports are timezone-aware** (`--timezone`, `format_date_tz`). Any stored aggregate that is
  keyed by local date is not re-bucketable by a viewer in another timezone. This drives the storage
  granularity decision in §5.2.
- **Docs, schema, snapshots, and `just` recipes are part of "done"** (`just fmt`, `just check`,
  `just test`, `just typecheck`, `just hawk`, docs guide + VitePress nav).

---

## 2. Research steps

### 2.1 Already carried out in this session (answers folded into the design)

| # | Question | Finding |
|---|---|---|
| R1 | Can GCS give us safe concurrent writes? | Yes. `ifGenerationMatch` on JSON API (`x-goog-if-generation-match` on XML API) gives compare-and-swap; `ifGenerationMatch=0` = create-if-absent; mismatch → `412`. |
| R2 | Is there an auth path that generalizes to other providers? | Yes. GCS HMAC keys + the XML API accept `AWS4-HMAC-SHA256` SigV4 against `https://storage.googleapis.com`. One signer then covers GCS, S3, R2, MinIO. |
| R3 | Can a bucket host the dashboard? | Yes — uniform bucket-level access + `roles/storage.objectViewer` for `allUsers`, `website.main_page_suffix=index.html`. Public URL `https://storage.googleapis.com/BUCKET/index.html`. |
| R4 | Do we already have z.ai prices? | Yes, embedded (GLM-5.3 $1.40/$4.40, cache read $0.26; GLM-4.6 $0.60/$2.20 per 1M) and refreshable from models.dev / LiteLLM at runtime. |
| R5 | Does ccusage know who the user is with the provider? | **No.** Nothing in `rust/` reads any account id — the adapters only parse log files. Claude Code keeps identity in `~/.claude.json` (`oauthAccount.emailAddress` / `accountUuid` / `organizationUuid`, plus `userID`), a state file Anthropic owns and can change; ccusage does not read it today. |
| R6 | What does bucket auto-create require? | `storage.buckets.create` in **an existing GCP project with billing**, plus a globally-unique bucket name. Scopes: `devstorage.read_write` for data, `cloud-platform` (or `devstorage.full_control`) for create + IAM. ADC from `gcloud auth application-default login` already carries `cloud-platform` and stores a refresh token locally. |

### 2.2 Still to do before implementation (each is a time-boxed spike)

- **S1 — GCS credential matrix (blocking, ~½ session).** Decide the supported credential kinds.
  Candidates: (a) HMAC key pair + SigV4 (small: `hmac`+`sha2`, no RSA, multi-provider); (b) service
  account JSON → RS256 JWT → OAuth token (needs an RSA signer: `rsa`/`ring` — measure binary-size
  delta with `cargo bloat`, cf. `rust-binary-size`); (c) ADC: shell out to
  `gcloud auth print-access-token`, or read `~/.config/gcloud/application_default_credentials.json`
  and do the refresh-token grant (no RSA); (d) GCE/Cloud Run metadata server. **Deliverable:** a
  size measurement for (b) and a recommendation. Working hypothesis: ship (a) + (c) in v1, add (b)
  only if the size cost is <150 KiB.
- **S2 — Object-count / cost / latency envelope.** How many objects does a year of one machine's
  usage produce under the layout in §5.3, and how many HTTP range/GET requests does the dashboard
  need on cold load? Target: dashboard first paint from ≤3 GETs. Validate GCS Class B op pricing
  and egress for a realistic user (2 machines × 18 months).
- **S3 — Machine identity stability.** Which fingerprint is stable across reboots/OS upgrades but
  not privacy-hostile: `/etc/machine-id` (Linux), `IOPlatformUUID` (macOS),
  `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (Windows), hostname fallback. Confirm behavior
  in containers/devcontainers/WSL (machine-id is often identical across containers from one image →
  collisions). **Deliverable:** decision on hashed-fingerprint + user-overridable `machineId`.
- **S4 — Same-logs-on-two-machines reality check.** How often are `~/.claude/projects` trees shared
  (Dropbox/iCloud/NFS/devcontainer bind mount)? Determines whether cross-machine dedup (§5.5) is a
  must-have or a safety net.
- **S5 — Browser CORS/exposure check.** Confirm whether a bucket-hosted dashboard reading sibling
  objects needs a CORS config at all (same-origin under `storage.googleapis.com/BUCKET/...` — but
  *not* same-origin under a custom domain + CDN). Also confirm public-access-prevention org policies
  commonly block the public path, so the private + signed-URL mode (§6.4) must exist.
- **S6 — Dashboard bundle delivery.** Embed a gzipped static bundle in the Rust binary
  (`include_bytes!`, budget ≤200 KiB compressed) vs. download from a GitHub release at deploy time.
  Measure a minimal Vite+TS+uPlot bundle. **Deliverable:** number + decision.
- **S7 — Model equivalence + subscription semantics.** Curate `claude-*` ↔ `glm-*` ↔ `gpt-*` ↔
  `gemini-*` tiers, and decide how to present plan-based pricing (Claude Max, GLM Coding Plan) next
  to per-token counterfactuals. Also confirm redistribution terms for models.dev/LiteLLM data when
  the prices are copied into a public bucket artifact.
- **S12 — Public-page/private-data split (blocking for §6.4).** Verify the conditional IAM binding
  (`allUsers` + `resource.name.startsWith(.../objects/<prefix>/dashboard/)`) actually works under
  uniform bucket-level access, what it costs in public-access-prevention terms, and how the
  browser-side GIS token flow behaves on the shared `storage.googleapis.com` origin (authorized-origin
  registration, token leakage between buckets). **Deliverable:** a working end-to-end probe bucket,
  or a decision to make mode B/C the default.
- **S9 — Can we ship a Google OAuth client in an OSS binary? (blocking for the "no gcloud" path)**
  Installed-app clients use PKCE + loopback and have no usable secret, which is fine, but
  `cloud-platform` is a sensitive/restricted scope: confirm whether an unverified client caps us at
  the test-user limit and what Google's verification/branding review costs. If verification is a
  blocker, the no-gcloud path degrades to "paste an HMAC key pair" and ADC stays the happy path.
- **S10 — Project selection UX.** A user with no GCP project cannot get a bucket. Confirm the
  cheapest guided path (`gcloud projects create` + billing link vs. deep-linking the console) and
  what the free-tier storage allowance covers for a typical year of shards.
- **S11 — Provider account identity sources.** Confirm the shape and stability of
  `~/.claude.json.oauthAccount` and the Codex/others equivalents (`~/.codex/auth.json` JWT claims),
  and how they behave for API-key users, Bedrock/Vertex users, and multi-account setups.
  **Deliverable:** the ordered identity-resolution chain in §4.0, with a fallback that never blocks
  sync.
- **S8 — Privacy defaults.** Enumerate what leaves the machine (project paths, session ids, git
  branch names via project dirs). Decide default redaction. Legal/compliance sanity check for
  corporate users.

---

## 3. Goals / non-goals

**Goals**
1. One-time `ccusage sync setup` that records *where* to sync and *how* to authenticate, provider-agnostic in shape, GCS-only in v1.
2. `ccusage sync` that is idempotent, incremental, safe under concurrent runs from multiple machines, and never loses data.
3. A single-page dashboard served from the bucket showing all machines/users in one view, plus a
   "what would this have cost on another provider" table.
4. The dashboard page is readable by anyone on the internet; the usage data behind it is readable
   only by the authenticated owner (and whoever they explicitly grant) — see §6.4.

**Non-goals (v1)**
- A server or a ccusage-run account system; viewer identity is Google's, authorization is bucket IAM.
- Real-time streaming sync (it is a batch command; a `--watch` mode is a later add).
- Syncing raw JSONL transcripts (prompt content never leaves the machine).
- Multi-tenant team aggregation across *different* people's buckets.

---

## 4. Feature 1 — `ccusage sync setup` (cloud target configuration)

### 4.0 Identity model: who is "the user", and where does the machine fit?

**How ccusage understands your usage today:** purely from local files. The adapters glob log
directories (`~/.claude/projects/**/*.jsonl`, `CLAUDE_CONFIG_DIR`, the equivalents for the other 17
agents), parse per-message token counts, and dedupe on `message.id` + `request_id` + `session_id`.
There is **no provider account id anywhere in the pipeline** — "your usage" currently means "the
logs in this home directory". That is exactly why sync needs an identity decision rather than
inheriting one.

**Resolution chain for `userId`** (first hit wins, all hashed before they leave the machine):

1. `sync.userId` in config — explicit, always wins, and is what a user sets when the automatic
   answer is wrong (shared laptop, work vs. personal account).
2. Provider account identity from the agent's own state file — for Claude Code,
   `~/.claude.json` → `oauthAccount.accountUuid`, else `oauthAccount.emailAddress` lowercased
   (pending S11 for the other agents). This is the only identity that is genuinely "you" across
   machines.
3. Fallback: `sha256(os_username + "@" + hostname_domain)` — stable enough to sync, and the setup
   flow tells the user to pin `sync.userId` if they sync a second machine.

Stored form is `sha256(source_value)[..16]` with a per-bucket salt, plus a plaintext `label` the
user chose. The raw email never lands in the bucket. Multiple provider accounts on one machine are
recorded as hashed `accounts[]` on the machine record so the dashboard can show "this usage came
from two Claude accounts" without revealing which.

**Bucket cardinality: one bucket per user, full stop.** The machine is a *shard key inside* that
bucket, not part of the bucket identity. Rationale:

- A bucket per (user, machine) makes the headline feature — one merged view across laptop, desktop
  and devcontainer — impossible without cross-bucket fan-out, extra IAM, and an index of buckets.
- Bucket names are a global namespace and bucket creation is a project-level, quota-bearing,
  billing-attached operation; minting one per machine is the expensive direction.
- Per-machine *objects* already give every isolation property we wanted from per-machine buckets:
  single-writer ownership, no write contention, per-machine retention and deletion (`sync forget
  <machineId>` deletes one prefix).

So the machine still matters, and matters a lot — as the shard key that makes concurrent writes
conflict-free (§5.3) and as a dashboard dimension — it just never selects the bucket.

`machineId` = user label if set, else `sha256(machine fingerprint)[..12]` (S3 picks the
fingerprint). Stored in config on first sync so it survives fingerprint drift; a changed
fingerprint is a warning, not a new machine.

### 4.1 Command surface

```
ccusage sync setup [--provider gcs] [--project ID] [--bucket NAME] [--location REGION]
                   [--auth auto|adc|oauth|hmac|service-account] [--non-interactive] [--json]
ccusage sync status            # resolved target, identity, credential source, last sync, pending shards
ccusage sync doctor            # preflight: creds, bucket reachable, write+CAS probe, clock skew
ccusage sync forget <machine>  # delete one machine's shards from the bucket
```

`setup` is interactive by default and is designed so the happy path is a single command with zero
flags and no console visits.

### 4.1.1 Zero-friction setup ladder

`--auth auto` walks this ladder and stops at the first rung that works:

1. **ADC already present** (`GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth application-default
   login` file, or a metadata server). Most developers with `gcloud` installed are already here and
   the whole flow is non-interactive. ADC user credentials carry `cloud-platform`, which covers both
   bucket create and object writes.
2. **`gcloud` installed but no ADC** → offer to run `gcloud auth application-default login` for the
   user (one browser consent, owned by Google's own verified client — no ccusage OAuth client
   needed).
3. ~~**No `gcloud`** → built-in browser flow.~~ **Cut from v1 by DR-05** — the scopes are
   sensitive, so an unverified ccusage client would be capped at 100 test users. Users without
   `gcloud` go to rung 4.
4. **Headless / CI / "just give me keys"** → paste an HMAC access id + secret. This is also the
   rung that makes the same code work for S3/R2 later. Service-account JSON is *not* accepted as
   implemented (DR-11): it would need an RSA signer in the binary, and the error says so and points
   at the HMAC rung instead.

Project selection: if exactly one project is visible, use it; otherwise show a picker; if none,
print the exact `gcloud projects create` + billing-link steps (S10). The chosen project is written
to `sync.projectId`.

### 4.1.2 Bucket auto-creation (idempotent, exactly one per user)

```
name := sync.bucket  ||  "ccusage-" + userIdShort + "-" + rand4     // lowercase, ≤63 chars
GET  b/<name>                    → 200 and we can write  ⇒ reuse, done
                                 → 404                   ⇒ create
POST b?project=<projectId>  { uniformBucketLevelAccess: true,          // conditions need it
                              publicAccessPrevention: "inherited",     // see DR-11
                              location: <user choice, default multi-region of their project>,
                              softDeletePolicy: 0, versioning: false }
                                 → 409 owned by someone else ⇒ retry with a fresh rand4 suffix
                                 → 403                        ⇒ explain the missing role, exit
```

The resolved name is written back to `sync.bucket`, so it is resolved once and reused forever; a
second machine for the same user reads the same config value (or is given the bucket name by the
user) rather than creating anything. `sync setup` refuses to create a second bucket when
`sync.bucket` is already set and reachable — `--recreate` is the explicit escape hatch.

### 4.2 Config shape (`ccusage.json`, non-secret only)

```jsonc
{
  "sync": {
    "provider": "gcs",                    // enum today: "gcs"; reserved: "s3", "r2", "azblob"
    "projectId": "my-gcp-project",
    "bucket": "ccusage-9f3a1c2b-7q4d",   // resolved once by `sync setup`, then permanent
    "prefix": "ccusage/v1",
    "auth": { "kind": "auto" },           // "auto" walks §4.3; "hmac" pins the env pair
    "machineId": "9f3a1c2b…",             // random 128-bit, minted once (DR-03)
    "machineLabel": "laptop-work",        // optional display name
    "userId": "7q4d…",                    // from the bucket manifest, or minted (DR-04)
    "agents": ["claude", "codex"],          // default: all detected
    "redactProjects": true,                 // default true
    "dashboard": { "deploy": true, "public": true, "encrypt": false }  // see §6.4: public page, private data
  }
}
```

Precedence stays the existing one: CLI flag > `commands.sync` > `defaults` > built-in. The block is
added to `config-schema.json` (generated; snapshot tests in `ccusage-config/src/snapshots` update
with it).

### 4.3 Credentials — never in the config file

Resolution order as implemented in P1-05, first hit wins:
1. `CCUSAGE_SYNC_ACCESS_TOKEN` — a pre-minted bearer token, for CI.
2. `CCUSAGE_SYNC_HMAC_ACCESS_ID` + `CCUSAGE_SYNC_HMAC_SECRET` — both or neither; half a pair is an
   error rather than a silent fall-through to the next rung.
3. `GOOGLE_APPLICATION_CREDENTIALS`. An explicitly configured path that fails is an error, never a
   fall-through: falling back would use an identity the user did not ask for.
4. The well-known ADC file (`~/.config/gcloud/application_default_credentials.json`).
5. `gcloud auth print-access-token`.
6. The metadata server, when one is configured or reachable.

Authorized-user ADC files are exchanged for an access token over the refresh-token grant; tokens
are cached until 60s before expiry. Service-account and external-account files are rejected with
remediation (DR-11). Exhausting the ladder produces an error that names every rung tried, since
"no credentials" with no provenance is unactionable. An OS keychain rung (P1-07) is optional and
not yet built.

`ccusage sync setup` writes the non-secret block to config and tells the user exactly which env var
or keychain entry to populate; it never echoes a secret and never writes one to the repo-local
`.ccusage/ccusage.json`. `.gitleaks.toml` already exists — add a rule for HMAC access ids.

### 4.4 Provider abstraction (future-proofing without over-building)

New crate `ccusage-objectstore` (no TLS/HTTP dependency):

```rust
pub struct ObjectMeta { pub key: String, pub generation: Option<String>, pub etag: Option<String>,
                        pub size: u64, pub updated: Option<TimestampMs> }
pub enum Precondition { None, IfAbsent, IfGenerationMatch(String) }

pub trait ObjectStore {
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, ObjectMeta)>>;
    fn put(&self, key: &str, body: &[u8], content_type: &str, pre: Precondition)
        -> Result<ObjectMeta>;                 // Err(Conflict) on 412
    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;
    fn delete(&self, key: &str, pre: Precondition) -> Result<()>;
}
```

`GcsStore` (SigV4/XML or Bearer/JSON) implements it inside the binary crate next to `http.rs`, keeping
`ureq`/rustls out of every adapter's build, exactly as pricing does. A second provider later is a new
`impl` plus one `provider` enum arm — the sync engine itself is provider-blind. Deliberately *not*
generalized in v1: multipart/resumable uploads, server-side copy, lifecycle rules.

---

## 5. Feature 2 — `ccusage sync` (merge local ⇄ cloud)

### 5.1 Constraints to design around (the "research the constraints first" ask)

1. **Data is per (user, machine, agent).** Two machines produce disjoint logs; the cloud view is a
   *union*, not a merge of conflicting values — unless the same log tree is visible on both.
2. **Local logs are append-mostly but not append-only.** Claude Code rewrites/compacts files;
   entries can arrive late (a session running past midnight, a machine offline for a week).
   ⇒ "already synced this date" is not sufficient; we need content hashing.
3. **Costs are not stable.** `--mode auto/calculate/display`, pricing refreshes, and
   `pricingOverrides` all change the cost of the *same* tokens. ⇒ store **tokens as the source of
   truth** and cost as a derived, labelled value.
4. **Dates are local-time buckets.** The uploader's `--timezone` must not dictate the viewer's.
   ⇒ store timezone-independent buckets (§5.2).
5. **Concurrency is real.** Two machines can sync in the same second, and a laptop can sync twice
   in parallel. ⇒ CAS preconditions + single-writer ownership of each mutable object.
6. **Privacy.** Project directory names leak client/repo names. ⇒ redaction by default.
7. **Cost/latency.** Object count and browser fetch count matter (S2).
8. **Clock skew / wrong system clock** on one machine can produce future-dated buckets.
   ⇒ `doctor` checks skew against the `Date` response header and warns.

### 5.2 Storage granularity decision

Options considered:

| Option | Merge fidelity | Size | Privacy | Re-bucketable by tz | Verdict |
|---|---|---|---|---|---|
| Raw JSONL mirror | perfect | huge | worst (prompts) | yes | rejected |
| Per-entry records (id + tokens + model + ts) | perfect | large (10⁵–10⁶ rows/yr) | ok | yes | rejected for v1 size/cost |
| **Per-(machine, agent, UTC-15-min bucket, model) rollups + session index** | exact dedup within a machine, near-exact across | small | good | **yes** | **chosen** |
| Per-local-day aggregates | lossy | tiny | good | no | rejected (constraint 4) |

15-minute UTC buckets are the smallest unit that can be re-aggregated into *every* real IANA
offset (including `+05:30`, `+05:45`) without splitting an interval. A day is then 96 buckets, and
most are empty — a sparse array keyed by bucket index compresses to a few KB/day.

Each **(bucket, model) cell** also carries a **dedupe-key digest set** (the existing
`message.id`+`request_id` hash, salted, truncated to 64 bits, stored as a sorted array) so
cross-machine duplicates can be detected without shipping per-entry rows. Keys are attached to the
cell rather than to the shard so that an intersection identifies *which* tokens are duplicated — a
shard-level set would only say "something in this day overlaps", which is not actionable without
over-subtracting (see §5.5).

Everything hashed — `userId`, project paths, dedupe keys — uses a **per-bucket random salt**
generated at `sync setup`, stored in the bucket's private prefix and mirrored in local config.
Without it, `sha256(project_path)` is trivially reversible by dictionary attack: the space of
plausible paths (`~/dev/<company>-<repo>`) is small.

### 5.3 Object layout (single writer per mutable object)

The bucket belongs to one user (§4.0), so the layout below has no user dimension above the machine;
`users/<userId>/` is kept only as a forward-compatible level for the optional team mode in §8.

```
<prefix>/
  manifest.json                              # multi-writer, CAS; tiny: list of machines + schema version
  users/<userId>/                            # exactly one in v1
    machines/<machineId>/
      machine.json                           # single-writer: label, os, fingerprint, last sync
      shards/<agent>/<YYYY>/<MM>/<DD>.json   # single-writer, immutable-after-finalize
      index.json                             # single-writer: per-shard contentHash + finalized flag
  rollup/
    daily.json  weekly.json  monthly.json    # multi-writer, CAS, derived (rebuildable)
    models.json                              # per-model token totals across everything
  dashboard/                                 # index.html, assets/*, deployed by the CLI
```

Key property: **every mutable object except `manifest.json` and `rollup/*` is owned by exactly one
machine**, so the common path has zero contention. `manifest.json` and the rollups are updated with
read → modify → `ifGenerationMatch` → retry (bounded, jittered, ≤5 attempts), and both are fully
reconstructible from the shards via `ccusage sync repair`.

Shard body sketch:

```jsonc
{
  "schema": 1, "agent": "claude", "machineId": "…", "userId": "…", "utcDate": "2026-09-17",
  "generatedAt": "2026-09-17T18:12:03Z", "ccusageVersion": "20.0.21",
  "costMode": "auto", "pricingSnapshot": "litellm@2026-09-16",
  "buckets": [ { "i": 54, "m": "claude-sonnet-4-5",
                 "in": 1200, "out": 830, "cw": 4000, "cr": 91000, "cost": 0.0421, "msgs": 3,
                 "k": ["…base64 64-bit dedupe keys for this cell…"] } ],
  "sessions": [ { "id": "…", "project": "sha256:ab12…", "first": 1758…, "last": 1758…, "cost": 1.23 } ],
  "dedupe": { "algo": "fxhash64/v1", "salt": "bucket-salt/v1", "count": 812 },
  "contentHash": "sha256:…"   // over everything above, sans this field
}
```

### 5.4 Sync algorithm

```
1. resolve config + credentials; acquire a local lock file (one sync per machine at a time)
2. load local entries via the existing adapters (tokens only; cost recomputed, not trusted)
3. fold into (agent, utcDate) shards; compute contentHash per shard
4. GET my own index.json (single object) → diff contentHashes
5. for each changed shard:  PUT with ifGenerationMatch(<known gen>)  (or ifAbsent when new)
      - skip shards whose utcDate is finalized AND hash unchanged
      - a finalized shard whose hash changed → warn + overwrite, record in `anomalies`
6. PUT index.json (single-writer, CAS on my own generation to catch a second local process)
7. CAS-update manifest.json to register this machine (no-op if already present)
8. recompute rollups:  GET all machines' index.json + only the shards whose hash changed since the
   rollup's `basedOn` map → merge → CAS-PUT rollup/*.json  (retry from step 8 on 412)
9. optionally deploy/refresh dashboard assets (§6.5)
```

Flags: `--dry-run`, `--since/--until`, `--agent`, `--force` (re-upload finalized shards),
`--pull` (write cloud state into a local cache for offline `--from-cloud` reports), `--repair`
(rebuild manifest/rollups by listing), `--prune` (apply retention).

**Finalization:** a shard for a UTC date older than `now - 48h` is marked `finalized: true`; late
edits after that are the anomaly path, not the normal path. 48h covers the longest plausible
timezone + late-write window.

### 5.5 Merge semantics (precise)

- **Union across machines.** Totals = Σ over machines of per-bucket tokens. No conflict possible
  because shard keys include `machineId`.
- **Within a machine, last-writer-wins at shard granularity**, and the writer is the machine itself,
  so LWW is really "latest local scan wins" — correct, because the local scan is authoritative for
  that machine's logs.
- **Cross-machine duplicate detection (safety net).** Dedupe key sets are stored per
  (bucket, model), not per shard, so an intersection can be acted on without guessing which tokens
  it covers. When two machines' key sets for the same (agent, date, bucket, model) cell intersect:
  *total* overlap ⇒ suppress the cell on the machine with the lexicographically larger `machineId`
  and record `duplicateSuppressed`; *partial* overlap ⇒ suppress nothing and record
  `duplicateSuspected` with the overlap ratio. Subtracting a whole cell on partial overlap would
  silently delete genuine usage that merely shares a 15-minute window — a downward-biased error in
  a spend report, which is the worst direction to be wrong in. With 64-bit keys and ~10⁵
  entries/day, false-positive probability is negligible (<3e-10 per pair). Both outcomes surface in
  the dashboard as notes rather than silently.
- **Same machine re-identified** (new `machineId` after an OS reinstall) is *not* auto-merged;
  `ccusage sync merge-machine <old> <new>` does it explicitly.
- **Costs** are always recomputed at read time from stored tokens with the dashboard's chosen
  pricing; the shard's `cost` field is a convenience snapshot labelled with `pricingSnapshot`.
- **Deletions** never happen implicitly. `--prune` is explicit and retention-driven.
- **Schema evolution:** every object carries `schema`; a newer writer never rewrites older-schema
  shards in place — it migrates on read and writes forward at the current version.

### 5.6 Failure modes and their handling

| Failure | Handling |
|---|---|
| 412 on CAS | bounded retry with jitter; then `sync --repair` hint |
| Partial upload (process killed) | shards are independent + hash-verified; index.json is written last, so a crash leaves unreferenced-but-valid objects that the next run re-PUTs |
| Credentials expired | clear error naming the resolved credential source, exit 3 |
| Bucket unreachable / offline | exit non-zero, no local state mutated; `--offline` guard mirrors the pricing flag |
| Clock skew > 5 min | warn in `doctor` and in `sync` output |
| Two local processes | local lock file under `$XDG_STATE_HOME/ccusage/sync.lock` |

---

## 6. Feature 3 — dashboard in the bucket

### 6.1 What it is

A static, dependency-light SPA (`apps/ccusage-dashboard`, Vite + TypeScript, uPlot or hand-rolled
SVG for charts — no heavy framework; the monorepo is pnpm + strict supply-chain settings, so each
dependency has a real cost). It is deployed by the CLI into `<prefix>/dashboard/` and reads
`../rollup/*.json` + `../manifest.json` at runtime. No build step for the user; no server.

### 6.2 Single-view layout

- Header: totals (tokens, cost, sessions, active days), date-range picker, timezone selector
  (re-buckets from the 15-min buckets client-side), machine + agent filters.
- Cost/tokens over time (stacked by agent, toggle to stack by model or machine).
- Model breakdown table: tokens in/out/cache-write/cache-read, cost, share.
- Per-machine and per-project panels (project column shows hashed ids unless redaction was off).
- Data freshness per machine (from `machine.json`), plus an anomalies strip (duplicate suppression,
  late edits, missing pricing).

### 6.3 Provider comparison table (the "what if I used z.ai" view)

For every model actually used, compute counterfactual spend on each comparison provider:

```
cost_target = Σ_models  in·P_in(target) + out·P_out(target)
                       + cache_write·P_cw(target, fallback P_in)
                       + cache_read·P_cr(target, fallback P_in)
```

- **Prices** come from the existing embedded/refreshed catalogs (models.dev + LiteLLM), emitted by
  the CLI into `rollup/pricing.json` at deploy time so the browser needs no third-party fetch.
- **Equivalence map** (`model-equivalence.json`, curated, versioned in-repo): tiers such as
  `frontier` (claude-opus / gpt-5.x / gemini-pro / glm-5.3), `workhorse` (claude-sonnet / glm-4.7 /
  gpt-mini), `fast` (claude-haiku / glm-flash). The UI shows the mapping and lets the user override
  per row — the comparison is only as honest as this map.
- **Caveats rendered in the UI, not hidden:** providers price cache writes differently (z.ai
  publishes cached-input but no cache-write rate → we charge cache-write at input rate and say so);
  token counts are not portable across tokenizers (±10–20%); subscription plans (Claude Max, GLM
  Coding Plan) are shown as a separate flat-rate row ("your usage vs. plan price"), not mixed into
  per-token math.
- Output: a table of `provider → counterfactual cost → Δ vs. actual → %saving`, sortable, plus a
  headline "you would have paid $X (−Y%) on <provider>".

The same computation is also available headless: `ccusage compare --provider zai [--json]`, so the
number is testable in Rust with snapshot tests rather than only in the browser.

### 6.4 Access model: public page, private data

> **Superseded by DR-12.** GCS rejects an IAM condition on `allUsers`, so the single-bucket split
> described below is not implementable. The page lives in its own `<data-bucket>-dashboard` bucket.
> The asymmetry this section argues for is unchanged; only the mechanism is.

The requirement is an asymmetry — **the web page is world-readable, the usage data is not**. The
bucket is therefore split into two access domains:

| Prefix | Contents | Access |
|---|---|---|
| `<prefix>/dashboard/**` | `index.html`, JS/CSS, static assets | public read (`allUsers:objectViewer`) |
| everything else (`manifest.json`, `users/**`, `rollup/**`) | the actual usage data | private — IAM principals only |

With uniform bucket-level access this is one **conditional** IAM binding: `allUsers` →
`roles/storage.objectViewer` with `resource.name.startsWith("projects/_/buckets/<B>/objects/<prefix>/dashboard/")`,
and public access prevention left `inherited` for the bucket (DR-11; S12 verifies the condition
syntax and that no org policy forbids it). Nothing else in the bucket is reachable without credentials, so an
anonymous visitor gets a working page that renders an empty state and a "Sign in" button.

**Primary data path — viewer signs in (real IAM).** The page runs a Google Identity Services
token flow (PKCE, scope `devstorage.read_only`), then reads the private objects through the JSON
API with `Authorization: Bearer`. Access is decided by bucket IAM: the owner sees everything,
anyone they grant `objectViewer` to (a teammate, a second Google account) sees it too, everyone
else gets 403 and the empty state. The access token is held in memory only, never persisted.

Two constraints this creates, both real:

- **Shared-origin hazard.** Served from `https://storage.googleapis.com/<bucket>/...`, *every*
  public bucket shares one browser origin, so `localStorage`/`sessionStorage` is not a safe place
  for a token and OAuth "authorized JavaScript origins" cannot distinguish our page from anyone
  else's. Mitigations: keep tokens in memory only, and recommend a custom domain
  (`usage.example.com` via a load balancer or Cloudflare) for anyone who wants the sign-in path as
  their normal mode. On the shared origin the CLI prefers mode B or C below.
- **CORS.** Same-origin when page and API are both `storage.googleapis.com`; a custom domain makes
  the API call cross-origin, so `sync dashboard` writes the bucket CORS config for that domain.

**Mode B — signed-URL link (no viewer sign-in).** `ccusage sync dashboard --share [--ttl 24h]`
mints V4 signed URLs for the data objects and hands back one link carrying them in the URL
*fragment* (never sent to the server, never in bucket logs). Anyone with the link reads the data
until it expires; the page itself stays public and useless without a fragment. Signing is pure
HMAC-SHA256 with HMAC credentials, or one `signBlob` IAM call with ADC.

**Mode C — client-side encryption (public objects, private content).** `sync.dashboard.encrypt =
true` encrypts every data object with AES-GCM under a key derived (Argon2id/PBKDF2) from a
passphrase that never leaves the machine; ciphertext can then sit in the public prefix. The page
asks for the passphrase and decrypts with WebCrypto. This is the only mode that works when org
policy forbids handing anonymous users any bucket-level read, and the only one with no Google
account involved on the viewing side. Cost: no key recovery, and re-keying means re-uploading.

Default: **public page + private data + viewer sign-in (mode A)**, with `--share` available
ad hoc. `sync.dashboard.public = false` keeps even the page private (signed URL for everything) for
users who want nothing world-readable at all. Publishing the page prints exactly what becomes
public and requires `--i-understand-public` on first deploy; a random 8-char prefix suffix keeps
the URL non-enumerable.

### 6.5 Delivery of the bundle

Preferred (pending S6): the gzipped bundle is embedded in the Rust binary via `include_bytes!` from
`OUT_DIR` (same pattern as the deflated pricing snapshots) with a **≤200 KiB compressed budget**
enforced by a test, and uploaded with correct `Content-Type` + `Cache-Control` + a content-hash in
asset filenames. Fallback if the budget cannot be met: download the versioned bundle from the
matching GitHub release at deploy time (breaks `--offline`, hence second choice).

---

## 7. Implementation plan

Each phase is independently shippable and independently revertable (squash-merged PRs, Conventional
Commits, TDD per the repo's `tdd` skill).

The phase summaries below are the shape; the discrete, dependency-ordered task list with
acceptance criteria lives in `specs/cloud-sync-tasks.md` (task IDs `P0-01` … `P6-05`).

**Phase 0 — spikes (S1, S3, S6 are blocking).** Output: a short decision record appended to this
spec. ~1 session.

**Phase 1 — object store + GCS client.**
`ccusage-objectstore` crate (trait, key builder, retry/backoff, error taxonomy) + `GcsStore` in the
binary crate with SigV4 signing and/or Bearer tokens, `ifGenerationMatch` support. Tests: signature
vectors against Google's documented examples, a mock HTTP store, 412 retry behavior. ~1 session.

**Phase 2 — `ccusage sync setup` / `status` / `doctor`.**
Parser + `SyncArgs` + help JSON + config block + `config-schema.json` regeneration + snapshots +
identity resolution (§4.0) + credential ladder (§4.1.1) + idempotent bucket creation (§4.1.2) +
docs page. No data movement yet. ~1–1.5 sessions.

**Phase 3 — shard model + local sync engine.**
`ccusage-sync` crate: bucket folding, dedupe digests, contentHash, index/manifest CAS, rollups,
`--dry-run`, lock file, anomalies. Fixture-backed tests plus a fake `ObjectStore` that can inject
412s and partial failures. ~1–2 sessions.

**Phase 4 — `ccusage compare` (headless provider counterfactual).**
Equivalence map + computation + `--json` + snapshot tests. Lands before the UI so the UI has a
verified oracle. ~1 session.

**Phase 5 — dashboard app + access split.**
`apps/ccusage-dashboard` + build wiring + size budget test + `sync dashboard` deploy, the
public-page/private-data IAM split, the browser sign-in path, and `--share` signed links.
Mode C (client-side encryption) lands only if S12 forces it. ~2 sessions.

**Phase 6 — release readiness.** Real-bucket end-to-end run, performance and binary-size review,
security review and threat model, docs index/README cross-links. Note that the per-feature docs are
not deferred to this phase — each of phases 2–5 writes its own guide, because docs written three
phases after the behavior are wrong docs. See `specs/cloud-sync-tasks.md` phase 6.

Validation each phase: `just fmt`, `just check`, `just test`, `just typecheck`, `just hawk`; CLI
snapshot updates; `LOG_LEVEL=0` for captured output.

---

## 8. Risks and open questions

- **Binary size** — the whole point of the http.rs seam. Watch RSA/keyring/bundle; budget each.
- **The public/private split is only as strong as two things:** one conditional IAM binding (S12)
  and the invariant that no data-bearing object key ever starts with the public `dashboard/`
  prefix. Both need to be enforced in code and tested, not merely documented — one convenience
  commit that parks a rollup next to the app publishes the user's spend.
- **Equivalence map is editorial.** It will be argued about; make it data, overridable, and dated.
- **Tokenizer non-portability** makes the savings number indicative, not a quote. Must be stated in
  the UI itself.
- **GCS-only v1** — the `provider` enum and `ObjectStore` trait are the only guards against a
  GCS-shaped design leaking everywhere; keep GCS specifics behind them (reviewers should reject any
  `x-goog-*` outside `GcsStore`).
- **Open:** should `sync` also let a machine *pull* to produce local reports over merged data
  (`ccusage daily --from-cloud`)? Cheap once shards exist; adds a second read path to maintain.
- **Open:** retention/pruning policy defaults (keep forever? 24 months?).
- **Open:** do we want team mode (several users → one bucket) in the layout now? The layout already
  has `users/<userId>/`, so it is possible, but IAM/ACL guidance would need writing.

---

## 9. Decision Record (Phase 0 spikes)

Dated 2026-09-17. Each entry records what was probed, what was found, and what the finding changes
in this design. Where a decision contradicts an earlier section, **this section wins** and the
affected task in `specs/cloud-sync-tasks.md` is annotated.

### DR-03 — Machine identity (P0-03 / S3)

Probed: `/etc/machine-id` and `/var/lib/dbus/machine-id` on this Linux host (both present,
identical, 32 hex chars); DMI `product_uuid` (absent — unreadable without root even where it
exists). Surveyed the documented behavior of `IOPlatformUUID` (macOS, IOKit) and
`HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (Windows).

Every hardware-rooted source has a collision or churn mode that matters for us:

| Source | Collides when | Churns when |
|---|---|---|
| `/etc/machine-id` | VM/VPS cloned without truncating it; container images that bake a non-empty file | stateless boots regenerate per boot |
| `MachineGuid` | every process-isolated Windows container from one base image | OS reinstall |
| `IOPlatformUUID` | — | logic board replacement |

Collisions are the dangerous direction: two machines sharing an ID silently interleave writes into
one shard and corrupt the dedupe accounting. Churn only fragments the dashboard.

**Decision.** The machine ID is **not** a hardware fingerprint. `ccusage sync setup` generates a
random 128-bit ID and persists it in the local config; that is the shard key. A hardware
fingerprint is recorded *alongside* it purely to detect a copied config (fingerprint changed but ID
did not → warn, offer `--new-machine-id`). This removes the clone-collision class entirely, needs
no root, leaks no hardware serials, and keeps a `sync.machineId` override for people restoring a
backup who want continuity. Machine IDs are still salted-hashed before they reach an object key.

### DR-04 — User identity (P0-04 / S11)

Probed: the documented shape of `~/.claude.json` and `~/.codex/auth.json`. Two findings kill the
"read the provider's identity" plan as a *default*:

1. **It is not reliably there.** `oauthAccount` is absent for API-key users, for Bedrock/Vertex
   users, and — per anthropics/claude-code#57026 — for Windows desktop SSO users, where only an
   opaque `userID` hash is written. Codex keeps identity inside a JWT in `auth.json`.
2. **Reading it means reading live credentials.** `~/.codex/auth.json` holds `access_token` and
   `refresh_token`; `~/.claude.json` can hold `primaryApiKey`. Parsing those files to extract an
   email puts ccusage's process in the blast radius of every credential in them, for a field we
   only use as a cross-machine join key.

**Decision.** The user ID is **bucket-derived, not provider-derived**. On the first machine,
`sync setup` mints a random user ID and writes it into the bucket manifest. On any subsequent
machine, `sync setup --bucket gs://…` reads the manifest and adopts the ID it finds. Cross-machine
merging therefore works because the machines share a *bucket*, which is the thing the user actually
authenticated to — no credential file is ever opened. §4.0's resolution chain is revised to:
explicit `sync.userId` → manifest of the configured bucket → freshly minted random ID. Provider
account linkage becomes an opt-in display-only field (`sync link-account`), not an identity source,
and P3-14's salt requirement still applies to everything derived from it.

### DR-05 — Built-in OAuth client (P0-05 / S9) — **no-go for v1**

`devstorage.read_only`, `devstorage.read_write` and `cloud-platform` are all classified
**sensitive** scopes. A public client requesting them needs Google's verification (domain
ownership, branding review, privacy policy, scope justification, demo video); until verified the
app shows the unverified-app interstitial and is capped at 100 test users. A CLI also cannot keep a
client secret secret, so the client would be a public installed-app client with PKCE.

**Decision.** Rung 3 of the auth ladder (built-in ccusage OAuth client) is **cut from v1**. The
ladder is ADC → assisted `gcloud auth application-default login` → HMAC/service account for
headless. The dashboard's browser sign-in inherits the same constraint: v1 ships the signed-link
mode (Mode B) as the default share path, with viewer Google sign-in only via a client the user
brings. Verification can be pursued later as a product decision; it is not an engineering blocker
to remove from the critical path. This also shrinks P2-04 and de-risks P0-02.

### DR-06 — Cost and object-count envelope (P0-06 / S2)

Modeled against list pricing (single-region Standard: $0.020/GB-month, Class A $0.005/1,000,
Class B $0.0004/1,000; free tier 5 GB, 5,000 Class A, 50,000 Class B per month).

Heavy user, 3 agents × 2 machines × 18 months: 3,285 daily shard objects, each ~35 KB raw
(96 sparse 15-minute cells × ~3 models) ≈ 115 MB, well inside the free storage tier. The cost
driver is writes, not bytes: a sync touches today's shards + index + affected rollups ≈ 10 Class A
operations, so 20 syncs/day across 2 machines ≈ 12,000 Class A/month — over the free tier, at
about **$0.04/month**. Dashboard cold load is 3 Class B GETs (manifest, daily rollup, models
rollup), meeting the ≤3-GET target.

**Decision.** Layout confirmed, with two rules the sync engine must honor because they are what
keeps the bill in cents: never rewrite historical shards on a routine sync, and **never `list` on
the hot path** (listing is Class A; the index object exists precisely so we do not have to).

### DR-07 — Dashboard bundle budget (P0-07 / S6)

Measured the actual candidate vendor payload: uPlot 1.6.32 IIFE 51,081 B raw / 22,009 B gzip,
its CSS 1,857 B / 772 B, Preact 10.26 11,211 B / 4,773 B, htm 1,265 B / 685 B. Vendor total
**≈ 28 KB gzip**, against a 200 KiB budget.

**Decision.** Budget is not a constraint on library choice. Set the enforced ceiling at **60 KB
gzip for the shell** (vendor + app JS + CSS) and treat data as a separate budget, so a regression
test can fail on accidental bloat long before 200 KiB. Vendor code is bundled, not
CDN-loaded — a public CDN on the dashboard page would leak viewer IPs and add a third-party
availability dependency for a 28 KB saving.

### DR-10 — Price data redistribution (P0-10 / S7)

ccusage already embeds LiteLLM's `model_prices_and_context_window.json`. LiteLLM is **MIT** outside
its `enterprise/` directory, which permits copying the price data into a world-readable bucket
object, on the condition that the copyright notice and permission notice travel with it.

**Decision.** Publishing prices is permitted. P4-05 must write the MIT notice as a sibling object
(`dashboard/licenses/litellm-LICENSE.txt`) and the dashboard must carry a visible attribution line
plus the snapshot date. The equivalence map stays editorial and separate from the licensed data.

### DR-11 — Bucket access shape as implemented (P1-06)

Implementing bucket administration forced three details the earlier prose left contradictory.

1. **Public access prevention is created `inherited`, not `enforced`.** `enforced` refuses the
   `allUsers` binding outright, so §4.1.2's original `enforced` default would have made the
   dashboard impossible without a later relaxation step that nothing owned. Containment comes from
   the *condition* instead: the binding is scoped to `KeySpace::public_prefix()`, which is the same
   string `dashboard_asset` builds keys from, so the grant cannot reach past the public prefix. A
   user who never publishes a dashboard can set `enforced` and lose nothing.
2. **Soft delete is created off (`0`), not 7 days.** The default keeps deleted objects billable for
   a week; on a bucket this small the surprise is worth more than the recovery window, and the data
   is reproducible from local logs.
3. **A create that loses a race re-reads rather than fails.** Two machines running `sync setup`
   against one name is normal, so 409 (mapped onto `Conflict` alongside 412) is resolved by reading
   the winner's bucket. A 403 is *not* folded into "absent": "someone else owns this name" and
   "this name is free" lead to opposite next steps.

Also settled here: share links are signed through the IAM Credentials `signBlob` API, so v1 needs
no private key on the machine and no RSA implementation in the binary. That is what lets DR-05's
cut stand without stranding headless users.

### DR-12 — Two buckets, because a condition on `allUsers` is rejected (supersedes DR-11.1)

The first real deploy against GCS failed outright:

```
LintValidationUnits/PublicResourceAllowConditionCheck Error: Conditions are not allowed on
public resources.
```

DR-11.1's containment — a public binding scoped by an IAM condition to `KeySpace::public_prefix()`
— is not expressible. GCS treats `allUsers` as a public resource and refuses any condition on it,
so the choice is a bucket that is entirely public or one that is not; a public *prefix* does not
exist as an enforceable thing.

**Decision.** The page moves to its own bucket, `<data-bucket>-dashboard`, created on demand by
`sync dashboard --deploy` with public access prevention `inherited` and an unconditional
`allUsers` object-viewer binding. It holds the shell, the published price table, and the
equivalence map, and nothing else. The data bucket is now created with public access prevention
**`enforced`**, which DR-11.1 could not allow: it no longer has to serve anything public, so it can
refuse to. Shards, manifests, salts, rollups, and `rollup/keys.json` stay there and are reachable
only through credentials or a time-boxed signed URL.

The boundary is therefore the thing GCS actually enforces (the bucket), checked twice: by the
bucket the upload goes to, and by the key space, where `dashboard_asset` remains the only
constructor yielding a public key.

### DR-13 — Setup provisions the signer, because a manual key is a broken feature

DR-12 left the published page readable only through a signed URL, and the only signer the code had
was an HMAC key the user created and exported themselves. Application-default credentials — what
`sync setup` arranges, and therefore what nearly everyone has — cannot sign a URL locally, so
`--deploy` published a page that was guaranteed to show nothing and `--share` failed with a message
telling the user to go and run `gcloud storage hmac create`. A feature whose happy path ends in a
manual credential step is not a feature.

The `signBlob` alternative settled above needs the caller to hold
`roles/iam.serviceAccountTokenCreator` on a signing account, which is a grant setup cannot always
make; it was offered and not chosen.

**Decision.** ccusage provisions the signer itself: it creates or adopts
`ccusage-dashboard@<project>.iam.gserviceaccount.com`, grants it `roles/storage.objectViewer` on
the data bucket only, mints exactly one HMAC key, and stores it in this machine's state directory
(`sync-signer.json`, mode `0600`) — not in `ccusage.json`, which is shared and whose loader rejects
secret-looking keys outright. `--share` and `--deploy` read that key. `sync remove` deletes the
keys, the account, and the local file.

The credential at rest is the cost of the choice: a long-lived key that can read the usage data.
It is scoped to one bucket, read-only, owner-only on disk, never printed, and deletable in one
command — which is a better trade than a feature nobody can use.

### DR-14 — Sharing is its own command, not a side effect of setup

DR-13 put provisioning inside `sync setup` and minted lazily on first `--share`/`--deploy`. Both
paths create an IAM principal as a side effect of a command the user ran for another reason, and
both bury a slow, failure-prone step (a new service account is not usable for up to minutes, see
the propagation wait) inside an otherwise fast one — so setup reported a scary partial failure to
users who only wanted sync.

**Decision.** `ccusage sync share` enables sharing, `--disable` revokes it, and neither setup nor
the dashboard provisions anything implicitly. `--deploy`/`--share` check for the key before they
publish anything and fail pointing at `sync share`; the local dashboard never needs it. Enabling
and disabling are both idempotent, so a bucket set up before this existed enables sharing at any
time, and the propagation wait belongs to the one command whose purpose is to wait for it.
Disabling deletes the account's HMAC keys, the account, and the local file, and touches no data.

## 10. Threat model (P6-04)

What an attacker has to hold, what holding it gets them, and what the code does about it. Each row
names the assertion that keeps it true, so a regression shows up as a failing test rather than as a
paragraph nobody re-read.

### 10.1 Assets

| Asset | Where it lives | Worst case if read |
|---|---|---|
| Usage data (spend, tokens, models, session counts) | data bucket, `users/**` and `rollup/**` | a complete picture of when and how much the user works |
| Project identities | hashed into shards; plaintext only if the user turns redaction off | which codebases the user works on |
| Per-bucket salt | `manifest.json`, and the local `ccusage.json` | with bucket read access, turns project hashes back into a guessable set |
| Dedupe keys (`rollup/keys.json`) | data bucket | provider message/request IDs, salted |
| Credentials (ADC token, HMAC key) | the OS credential locations, never the config file | full control of the bucket |
| Dashboard signing key | `sync-signer.json` in the state directory, mode `0600` | read of the usage data, and the power to mint links to it |
| Object *names* | the data bucket's listing | machine IDs, which agents are installed, and an exact day-by-day activity calendar |

### 10.2 Boundaries and what enforces them

- **Public page vs. private data is a bucket boundary, not a prefix.** GCS refuses an IAM condition
  on `allUsers` (DR-12), so the page lives in `<data-bucket>-dashboard` and the data bucket is
  created with public access prevention **enforced**. Checked twice: by which bucket an upload goes
  to, and by `KeySpace`, where `dashboard_asset` is the only constructor yielding a public key
  (`classifies_only_dashboard_assets_as_public`, `rejects_dashboard_asset_paths_that_climb_out_of_the_prefix`).
- **Nothing user-derived is published.** Everything `--deploy` uploads comes from the embedded
  price and equivalence data, not from the user's logs
  (`nothing_published_is_derived_from_the_user`).
- **Credentials never enter the config file.** Setup writes a bucket, project, prefix, two IDs and
  the salt; `config::from_args` rejects the file outright if a secret-looking key appears in it
  (`the_written_config_can_only_contain_non_secret_settings`). A config setup *creates* is `0600`,
  because the salt is in it (`creates_a_new_config_readable_only_by_its_owner`).
- **Output and errors are scrubbed.** `assert_no_secrets` fails on `ya29.` tokens, HMAC secrets,
  `X-Goog-Signature` query strings and planted values; it guards status text, status JSON, the
  credential ladder's rendering and the written config. Gitleaks carries matching rules.
- **The local dashboard is loopback-only and checks `Host`,** so a page on the internet cannot read
  a developer's rollups by rebinding DNS to `127.0.0.1`.
- **Project paths are hashed by default** (`redactProjects` defaults to true), with the salt as the
  pepper, so a bucket reader sees stable but opaque project identities.

### 10.3 Read-access blast radius — the part worth saying out loud

Granting a teammate `roles/storage.objectViewer` on the data bucket grants them *listing*, and the
object names alone are a disclosure even if they never download a byte: `users/<userId>/machines/<machineId>/shards/<agent>/<YYYY>/<MM>/<DD>.json`
tells them how many machines the user syncs from, which agents are installed on each, and exactly
which days each machine was used — a work calendar, including holidays, sick days and the weekend
that was not one. The numbers inside are the rest of it.

So **sharing is a signed link, not an IAM grant**. `ccusage sync dashboard --share` mints V4 signed
URLs for the four rollup objects only — not shards, not `keys.json`, not the manifest — and carries
them in the URL *fragment*, which browsers never send to the server and which therefore stays out
of bucket access logs. The link is a bearer token and the CLI says so when it prints one.

- **TTL:** 24h by default, 7d maximum (GCS's own V4 ceiling), `--ttl` rejected outside that and
  rejected entirely without `--share`.
- **Revocation:** a signed URL cannot be withdrawn individually. Early revocation is deactivating
  the HMAC key it was signed with, which invalidates every link signed by that key; the dashboard
  guide says this where a user minting a link will read it.
- **Rotation:** minting from a dedicated service account's HMAC key keeps that blast radius off the
  user's own credentials. Setup creates that account and key (DR-13), so the dedicated-account
  property holds by default rather than by the user's diligence.

### 10.4 Accepted risks

- **The salt is in `ccusage.json`.** Every machine writing to the bucket needs the same salt, and
  the bucket's own manifest carries it, so it is no stronger than bucket access; the file mode
  keeps it off a shared machine's other accounts. It is not a credential and grants nothing alone.
- **A shared bucket is all-or-nothing.** There is no per-machine or per-day ACL; a reader of the
  data bucket reads everything in it.
- **`storage.googleapis.com` is a shared origin.** The published page sits on it with every other
  public bucket, which is why the share path is fragment-carried signed URLs rather than a
  browser sign-in holding a token (DR-05).
- **Turning redaction off is honoured.** `redactProjects: false` uploads plaintext project paths;
  it is the user's call, made explicitly.

### Still open

- **P0-02 (public page / private data on a real bucket)** — partly answered the hard way: a real
  deploy disproved the conditional binding (DR-12). A full publish and read-back of the two-bucket
  shape still needs a billing-enabled GCP project. DR-05 reduces its blast radius, since the
  default share path no longer depends on browser Google sign-in working on
  `storage.googleapis.com`.
- **P0-01** (credential matrix + `cargo bloat` deltas) — partially pre-empted by DR-05 (no RSA
  needed for rung 3); the service-account RS256 question remains for headless users.
- **P0-08**, **P0-11** — not yet run; neither gates Phase 1.
