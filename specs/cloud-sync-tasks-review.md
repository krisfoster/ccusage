# Review of `specs/cloud-sync-tasks.md` (v1)

Self-review pass for consistency, sequencing, security and latent bugs, against
`specs/cloud-sync-and-dashboard.md`. 19 findings. Severity: 🔴 must fix before Phase 0 exits,
🟠 fix before the affected phase starts, 🟡 tidy-up.

The fixes marked *applied* are already folded into the task list (now v2) and the design doc.

---

## A. Correctness bugs in the underlying design (inherited by the tasks)

### A1 🔴 Cross-machine duplicate suppression over-subtracts — `P3-07`

Design §5.5 says: when two machines' dedupe key sets intersect for the same agent+date, subtract
*the intersecting buckets* from the machine with the larger `machineId`.

That is wrong. A 15-minute bucket is an aggregate of many entries; an intersection tells us
*some* entries are shared, not *which*, and not how many tokens they account for. Subtracting the
whole bucket removes genuinely distinct usage that happened in the same 15 minutes. The failure is
silent and biased downward — the worst kind for a spend dashboard.

The shard as specified cannot fix this, because it stores keys and totals but no mapping between
them. Three options, in increasing cost:

1. **Per-(bucket, model) key sets** — move the dedupe digest down from shard level to bucket level
   and store the per-key token cost implicitly by suppressing at bucket granularity *only when the
   intersection covers the whole bucket's key set*; otherwise report the overlap and suppress
   nothing. Cheap, exact when it acts, honest when it cannot.
2. **Per-key token attribution** — store `(key64, in, out, cw, cr)` for shards that overlap. Exact,
   but it is per-entry rows through the back door (the thing §5.2 rejected on size).
3. **Prevent rather than detect** — make shared log trees a setup-time error: `sync doctor` warns
   when the log root looks like a network/synced volume, and suggests a shared `machineId`. Pairs
   well with (1).

Recommendation: (1) + (3), with the dashboard showing "N% of buckets on <date> overlap between
machine A and B" rather than a silently adjusted total. Requires S4 (how common is this really?) to
be answered first — which is itself a missing task, see C2.

*Applied:* `P3-07` rewritten; acceptance criterion changed from "totals do not double" to
"suppression is exact or declines to act, never partial".

### A2 🟠 Hashed project names are brute-forceable, and no task owns the salt — `P3-01`/`P3-02`

`sessions[].project` is stored as `sha256(path)`. The space of plausible project paths is tiny
(`~/dev/<company>-<repo>`), so an unsalted hash is reversible by dictionary attack for anyone who
can read a data object. §4.0 specifies a per-bucket salt for `userId` but nothing establishes,
stores, or rotates it, and the project-hash path never mentions it.

*Applied:* new task `P3-14` — generate a per-bucket salt at setup, store it in the bucket
(private prefix) *and* in local config, use it for every hash that leaves the machine, and define
behavior when it is missing (refuse to sync rather than silently hash unsalted).

This matters far more under Mode C (encryption) and Mode B (signed links), where a wider audience
sees the objects.

### A3 🟠 No forward/backward schema compatibility story on the Rust side — Phase 3

`P3-01` versions every object and `P5-02` migrates on read *in the browser*. Nothing covers the
Rust reader meeting a shard written by a **newer** ccusage on another machine — which happens the
moment one machine auto-updates first. §5.5 says "a newer writer never rewrites older-schema shards
in place", but says nothing about an older reader.

*Applied:* new task `P3-13` — unknown-future-schema shards are read as opaque (counted in the
manifest, excluded from rollups with an explicit "N shards need a newer ccusage" note) rather than
erroring or, worse, being parsed leniently and undercounted.

---

## B. Sequencing conflicts

### B1 🔴 `P5-08` (deploy) cannot run before `P5-09` (the thing being deployed)

`sync dashboard` uploads the bundle, but the bundle-embedding task sits *after* it with no
dependency either way. Either order produces an unbuildable intermediate state.

*Applied:* `P5-08` now depends on `P5-09`, and `P5-09` gains a dev-mode escape (`--bundle-dir`) so
the deploy path is testable before embedding is finished.

### B2 🔴 Public-access-prevention is set to `enforced` at creation (`P2-06`) and never unset

`P2-06` creates the bucket with `publicAccessPrevention: "enforced"` — correct default — but
`P5-08` then needs an `allUsers` binding, which PAP blocks. No task flips it, and `P1-06`'s op list
has no PAP mutation.

*Applied:* PAP toggle added to `P1-06`; `P5-08` explicitly flips PAP to `inherited` *and* back on
downgrade, and fails with a clear message when an org policy pins it (which is exactly the S12
fallback trigger).

### B3 🟠 `P5-05` (browser sign-in) depends on the IAM split that `P5-08` performs

`P5-05` lists only `P0-02` and `P5-02`. It cannot be verified without a deployed public page and
the conditional binding.

*Applied:* `P5-05` now depends on `P5-08`.

### B4 🟠 `P2-04` (credential ladder) omits its gating spike

Ladder rung 3 is the built-in OAuth client, gated on S9 = `P0-05`, which is not listed as a
dependency.

*Applied:* dependency added.

### B5 🟡 `P3-01` should depend on `P0-06`

`P0-06` measures object count/cost/latency *for the layout `P3-01` implements*. If the answer is
"too many objects", `P3-01` changes shape. Currently `P0-06` gates nothing, which means it could be
skipped with no visible consequence.

*Applied:* dependency added.

### B6 🟡 `P3-08` depends on `P2-08` (doctor) when it actually needs `P2-06` (bucket resolution)

Overtight — it serialises the engine behind a diagnostic command.

*Applied:* dependency changed to `P2-06`.

---

## C. Coverage gaps

### C1 🔴 Spike S7 has no task, yet two tasks cite it

`P4-01` depends on `P0-08` for the equivalence map, but `P0-08` is S10+S8 (project UX + privacy).
S7 — the equivalence map *and the redistribution licence for models.dev/LiteLLM prices copied into
a public bucket* — has no task at all. The licence question is the sharper half: `P4-05` publishes
third-party price data into an internet-readable object, and its acceptance criterion says
"licence question from S7 answered" with no task producing that answer.

*Applied:* new `P0-10` (S7), `P4-01`/`P4-05` re-pointed at it.

### C2 🟠 Spike S4 has no task

S4 (how often is the same log tree visible to two machines) determines whether A1 is a must-have or
a safety net, and therefore how much of `P3-07` is worth building.

*Applied:* new `P0-11`, and `P3-07` gated on it.

### C3 🟠 Commands in the design have no implementing task

- `sync merge-machine <old> <new>` (§5.5) — *applied:* folded into `P3-09`.
- `--offline` guard (§5.6) — *applied:* folded into `P3-08`.
- Mode B with ADC needs an IAM `signBlob` call (§6.4) — `P1-06` had no such op. *Applied:* added.

### C4 🟡 `P2-01` is ambiguous about the subcommand set

"Parser tests cover every flag" does not say *which* subcommands exist in Phase 2. `sync`,
`forget`, `repair`, `merge-machine`, `dashboard` all land in later phases but must parse (or be
explicitly rejected) coherently from the first parser change, because the help JSON and snapshots
are regenerated once.

*Applied:* `P2-01` now defines the full subcommand grammar up front, with not-yet-implemented
subcommands exiting with a clear "available in a later release" message rather than a parse error.

---

## D. Security findings

### D1 🔴 Nothing enforces the public/private key-space boundary

The entire access model rests on one invariant: **no object containing usage data may have a key
under `<prefix>/dashboard/`**. Today that invariant lives only in prose. One convenience commit
("just put pricing.json next to the app so the page can fetch it unauthenticated") silently
publishes data. `P4-05` is already flirting with exactly this — it emits `rollup/pricing.json`, and
the natural follow-up is to move it under the public prefix.

*Applied:* new task `P5-11` — a key-space invariant enforced in code (the store wrapper rejects
writes of data-classified objects to the public prefix) plus a test, and `P4-05` now explicitly
states pricing data *is* publishable (it is third-party price data, not user data) and lives under
the public prefix by deliberate classification, not by accident.

### D2 🟠 `P2-08`'s CAS probe writes a scratch object with no stated location or cleanup

A write probe into an unspecified key can land anywhere, including the public prefix once that
exists, and can be left behind on crash.

*Applied:* probe key pinned to `<prefix>/.probe/<machineId>`, deleted in a guard, and covered by
the D1 invariant test.

### D3 🟠 Shelling out to `gcloud` (`P2-04`, `P1-05`) needs to be spelled out as a trust decision

`gcloud auth application-default login` and `gcloud auth print-access-token` are resolved from
`PATH`. Invoking them must be explicit (never silent), without a shell, with an absolute resolved
path, and the token must never be logged. It is a reasonable decision — it is *not* a reasonable
implicit one.

*Applied:* stated in `P2-04`'s acceptance criteria and in `P2-09`'s redaction test scope.

### D4 🟠 Mode B signed-link leakage is under-described — `P5-06`

The fragment keeps the URL out of server logs and `Referer`, which is the right call, but it does
not keep it out of browser history, clipboard managers, or chat previews. A link is a bearer token.

*Applied:* `P5-06` requires a default TTL measured in hours (not days), a visible expiry in the UI,
and `sync dashboard --revoke` (rotate the HMAC key / bump the prefix salt) as the panic button.
Revocation is otherwise impossible for a signed URL, which is the property people forget.

### D5 🟡 `P2-09` (secret hygiene) runs before the signing and keychain code exists

Its scope is `P2-04` only, but `P1-07` (keychain), `P5-06` (signed URLs) and `P4-05` all handle or
emit sensitive material later.

*Applied:* `P6-04` explicitly re-runs the hygiene checklist across everything, and `P2-09`'s test is
written as a reusable assertion helper rather than a one-off.

### D6 🟡 No task covers what an attacker who *reads* the bucket learns

Even fully "private", the bucket's object *names* leak a lot to any principal with `list`:
machine ids, agents used, and an exact activity calendar. Granting a teammate `objectViewer` for
the dashboard grants them that too. Worth one paragraph of threat model rather than a discovery
during review.

*Applied:* `P6-04`'s checklist now includes a written threat model covering read-access blast
radius, with the recommendation that sharing be done via Mode B links rather than IAM grants.

---

## E. Consistency between the two documents

### E1 🟠 The design doc's risk list contradicts its own §6.4

§8 still says "Public dashboard = public spend data. **Private + signed URL must be the default.**"
§6.4 (rewritten after your requirement) makes public-page/private-data + viewer sign-in the
default. A reader hitting §8 first gets the wrong model.

*Applied:* §8 risk rewritten to the real residual risk — that the public/private split rests on one
IAM condition and one key-space invariant.

### E2 🟠 §4.2's config example still shows `dashboard: { "deploy": true, "public": false }`

`public: false` as the shipped example contradicts the new default and would confuse the
schema work in `P2-02`.

*Applied:* example updated to `"public": true` with a comment pointing at §6.4, and
`"encrypt": false` added so the Mode C key exists in the schema from the start.

### E3 🟡 Phase 6 means two different things

Design §7 Phase 6 = "docs + polish". Task list Phase 6 = "release readiness", with docs distributed
into the phases that create the behavior (which is the better arrangement — docs written three
phases after the code are wrong docs).

*Applied:* design §7 Phase 6 renamed and pointed at the task list.

### E4 🟡 The effort estimate is understated

Summing the task sizes (S≈0.15, M≈0.5, L≈1.0 sessions) gives ~16 sessions, not the 9–12 stated.
Phase 5 alone is ~4.

*Applied:* estimate corrected to 15–18 sessions, with the caveat that Phase 0 could remove work as
easily as add it (a no-go on S9 or S12 deletes tasks).

---

## What I did not change

- **The 48h finalization window** (`P3-05`). Arbitrary, but any value is; it is configurable and the
  anomaly path catches the tail.
- **The `manifest.json` CAS hot spot** (`P3-04`). Fine at the stated scale (a handful of machines);
  it would need sharding at team scale, which is a non-goal.
- **Rollups being derived and rebuildable** (`P3-06`/`P3-09`). Correct as specified — the repair
  path is the thing that makes CAS failures non-fatal, and it is already a task.
- **The two-writer race in step 8 of §5.4** (rollups computed from a peer's mid-write index). It is
  benign: the next sync converges, and the dashboard shows a freshness timestamp per machine.

## Suggested order of fixes

A1 and C1 change what gets built, so they belong in Phase 0's exit review. B1/B2 and D1 are cheap
edits now and expensive discoveries later. Everything else can be absorbed as its phase starts.
