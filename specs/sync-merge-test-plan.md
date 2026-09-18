# Adversarial review of the bucket merge, and the test plan it argues for

Scope: everything between "this machine has local usage" and "the bucket's rollups are correct" —
`sync/run.rs` (fold, plan, upload), `sync/machine.rs` (index CAS), `sync/bootstrap.rs` (manifest,
salt), `sync/rollups.rs` (rollup pass), `sync/maintenance.rs` (repair, forget, prune,
merge-machine), and the `ccusage-sync` merge primitives (`fold`, `shard`, `rollup`, `duplicates`).

The question this asks is narrower than "are there tests": **which interleavings of failure and
concurrency can make the bucket disagree with the machines' logs, and would any existing test
notice?**

## 1. What the merge actually promises

The code is not transactional and does not claim to be. Naming the four different guarantees it
does make is what makes the gaps visible, because a test written against the wrong one passes while
the system is broken.

| Object | Write discipline | Guarantee |
| --- | --- | --- |
| `shards/<agent>/<date>.json` | `Precondition::None`, last write wins | Idempotent for identical content; the machine owns the prefix, so a lost write is re-made next run |
| `machines/<id>/index.json` | CAS on generation, 5 attempts | Linearizable per object; converges across processes |
| `machines/<id>/machine.json` | CAS on generation | Advisory metadata only; never read by the merge |
| `manifest.json` | `IfAbsent` for identity, CAS for the roster | Identity is write-once; roster converges |
| `rollup/daily.json` | CAS on generation, 5 attempts | Linearizable; recomputable from shards |
| `rollup/{keys,weekly,monthly,models}.json` | Unconditional write after daily | **Eventually** consistent — may be one pass behind |

So the honest statement of the contract is: *per-object linearizability, whole-command
non-atomicity, convergence on the next successful run, manual `sync repair` only when an object is
unreadable rather than merely stale.* Two of the bugs below are cases where the code does not even
meet that.

## 2. Findings

Ordered by what they cost the user. **All eight are fixed in this branch**; each finding below is
followed by what was done. The test matrices in §4 remain the plan for proving the merge as a
whole, and are not all built yet.

### B1 (release-blocking, fixed here) — the clock check refused every sync on a bucket older than an hour

`execute` read the bucket's clock from `manifest.json`'s `updated_ms`. The manifest is written once,
at setup, and never again by `sync run`, so `skew = now - setup_time` grew without bound: any
bucket set up more than an hour ago failed with *"this machine's clock is 10080 minutes ahead of the
bucket's"*, before a single shard was folded. `doctor` got this right — it writes a probe object and
reads the timestamp the store stamps on it — and `run` did not.

Nothing caught it because there is no test of `execute` at all (see G1): the clock helper was tested
against `check_clock`'s arithmetic, never against what the bucket would actually return a week
later. Fixed by taking the time from a fresh probe write, and regression-tested with a store clock
advanced seven days.

### B2 — one unreadable shard bricks the merge for every machine

`rollups::refresh` tolerates a shard the index promises but the bucket lacks (`summary.missing`) and
a shard from a newer ccusage (`skipped_newer`), but a shard whose body is corrupt propagates the
parse error out of `refresh`, which fails the whole run. A single truncated object — an interrupted
upload from an old client, a botched manual copy — therefore stops *every* machine from merging,
including the machines that have nothing to do with it, and the error names a key rather than a
remedy. The tolerated cases show the intended design; this one is an omission. It should be counted
as `unreadable`, excluded from totals, reported in the summary, and repaired by re-upload.

**Fixed:** counted as `unreadable`, left out of the totals, reported in the summary, and recorded as an
`unreadableShard` anomaly on `daily.json`; the other machines merge normally.

### B3 — `daily.json` is fatal where `keys.json` is self-healing

`read_key_index` treats missing, malformed, and future-schema key indexes as "start again" and the
next pass rebuilds them. `read_daily` treats malformed as fatal. Both are derived state that
`sync repair` rebuilds from shards, so the asymmetry is not principled — and the fatal branch is the
one users hit, because `daily.json` is the object with the most writers. Either make it self-healing
(rebuild from shards) or make the error say `ccusage sync repair` in the sentence. Today it says
neither.

**Fixed:** `read_daily` now rebuilds from the shards when `daily.json` cannot be parsed, and the summary says
so. A future schema is still left alone rather than overwritten.

### B4 — a day can be silently dropped from the upload plan

```rust
let Ok(hash) = shard.finish().map(str::to_string) else { continue };
```

A shard that cannot be hashed is skipped without landing in `uploads`, `unchanged`, `late_edits` or
`skipped`. The user is told "Nothing to sync" while a day of spend never leaves the machine. Every
other refusal in this path is reported; this one is invisible, which is the failure mode the rest of
the design works hard to avoid.

**Fixed:** the day is collected into `unhashable` and named in the summary, so a run never reports nothing to
sync over usage that did not leave the machine.

### B5 — a shard whose contents disagree with its key is dropped from the totals every pass

`daily.apply` derives the cell's identity from the shard *body* (`ShardRef::of(shard)`), while
`live` is built from the *index keys*. If the two ever disagree — a shard copied between machine
prefixes, a `merge-machine` interrupted between rewriting the object and rewriting the index — the
applied ref is absent from `live`, so `stale_refs` forgets it in the same pass that applied it. The
usage vanishes from the rollup, and re-reading it next pass makes it vanish again. Cheap fix: treat
a body/key mismatch as an anomaly and refuse the shard loudly.

**Fixed:** `load_shard` compares the body's `ShardRef` with the key it was read from and refuses a mismatch as
a `misplacedShard` anomaly instead of applying it.

### B6 — nothing stops two `ccusage sync run` processes on one machine

The user's accidental-double-run case. Per-object CAS means this cannot corrupt the index or the
daily rollup, and that is genuinely the hard part, but it is not the whole story:

- both processes write the same shard keys with `Precondition::None`, so the loser's body wins if it
  lands second; if the two processes read the logs at different moments, the bucket can end up
  holding the *older* body while the index records the *newer* hash. Self-healing (the next pass
  re-reads because the hashes differ) but untested, and it makes `based_on` churn forever until a
  run is done alone;
- both refresh rollups, each burning the other's CAS attempts; with `MAX_ATTEMPTS = 5` and no
  backoff, the honest worst case is one process failing with "the bucket's rollups kept changing".
  That message is correct advice but the situation is entirely avoidable;
- the wasted half is not free — a double run doubles GCS write ops and, on a large history, the
  rollup pass's `stale_refs` scan.

Recommendation: an advisory lockfile in the config directory with a stale-lock timeout, whose
failure message says another sync is running. It is not a substitute for CAS (other machines are
still concurrent) and must never be treated as one.

**Fixed:** an advisory lockfile at `$XDG_STATE_HOME/ccusage/sync.lock`, taken by the mutating half of `run`
and by `repair`/`forget`/`merge-machine`, released on drop, broken after an hour so a crash cannot
wedge the machine. `--dry-run` does not take it. It is not a substitute for CAS.

### B7 — `sync run` does not ensure the machine is on the roster

Registration happens in `setup` only, but `rollups::refresh` iterates the *roster*, deliberately not
a listing. So a machine dropped from the manifest — by a `forget` on another machine, or a manifest
restored from an older copy — keeps uploading shards that nothing ever reads, and reports success
while contributing nothing. `register_machine` is idempotent and costs one GET on the happy path;
`run` should call it.

**Fixed:** `execute` calls `register_machine` before uploading.

### B8 — `stale_refs` is quadratic

`live.contains(...)` inside a loop over `daily.based_on`. At five machines × eighteen agents × a
year the comparison count reaches the tens of millions per pass. A performance cliff, not a
correctness bug, but it lands on exactly the users with the most data. `live` should be a
`BTreeSet`.

## 3. Where the existing tests are strong

Worth saying plainly, because the plan below is about the edges rather than the middle: fold and
shard hashing (order independence, stable hash across regeneration, day splitting, redaction),
incremental-equals-full-recompute, shrunken shards, cross-machine dedupe including the
partial-overlap-flags-rather-than-suppresses rule and restoration on forget, late edits and
settlement, newer-schema and missing-shard tolerance, and the maintenance commands are all
genuinely covered — 59 tests, 85–99% of those files.

The weakness is structural and uniform: **almost every test exercises one function, with one
injected fault, from a hand-built starting state.** There is no test that runs a whole sync, no test
with two processes, and no test that asserts the bucket's total equals the machines' local totals.

## 4. Test plan

Priority: **P0** blocks a release that people trust with their data; **P1** is the difference between
"we believe it works" and "we know what it does"; **P2** is hardening.

**Fixed:** `live` is a `BTreeSet`.

### 4.0 Test-double work this needs first (P0)

`MemoryStore` today takes a global FIFO of faults and has no way to say "fail the third PUT to this
prefix". Every scenario below that names a boundary needs:

- **keyed faults** — `fail_on(match, fault)` where the match is a key predicate and an operation
  kind, so a test can fail the write of `rollup/keys.json` specifically;
- **lost responses** — `succeed_then_fail(match)`: apply the write, then return `Network`. This is
  the case the current suite never models, and it is the common one on real networks;
- **a barrier hook** — `before(match, callback)` so a test can run the other process's step at an
  exact point, replacing the bespoke `RacingStore` wrappers in `machine.rs` and `bootstrap.rs`;
- **`advance_clock_ms`** (added in this branch) for age-dependent behavior;
- **op counting per key**, for asserting "the second run read no shards".

`MemoryStore` is already `Sync`, so real two-thread tests are possible once the barrier exists.

### 4.1 Scenario matrix: what state the bucket is in

| # | Starting state | Local state | Must be true after one run | Pri |
| --- | --- | --- | --- | --- |
| S1 | Empty bucket | Usage across 3 days, 2 agents | Every day uploaded and indexed; rollup totals **equal the sum of the local entries**; no public key written | P0 |
| S2 | Empty bucket | No usage at all | No objects beyond manifest/salt; summary says nothing to sync; no empty index | P1 |
| S3 | Populated | Identical logs | Zero PUTs of shards; zero shards read by the rollup pass; `daily.json` byte-identical apart from `generatedAt` | P0 |
| S4 | Populated | One day gained entries | Exactly that shard re-uploaded, exactly that shard re-read, other days' cells untouched | P0 |
| S5 | Populated | One day *lost* entries (logs pruned) | Cells removed, totals drop, no ghost cells left behind | P1 |
| S6 | Populated by machine A | Machine B joins with disjoint usage | Both machines' cells present, roster has both, totals are the sum | P0 |
| S7 | Populated by A | B has the *same* entries (shared log dir) | Counted once; suppression lands on the lexicographically larger machine ID from either machine's run | P0 |
| S8 | Populated by A | B partially overlaps A | Nothing suppressed, overlap flagged with a percentage | P0 |
| S9 | Populated | A settled day is rewritten | Uploaded, counted as a late edit, and the anomaly reaches `daily.json` | P1 |
| S10 | Populated, roster missing this machine | Any | B7: run re-registers, or (today) fails loudly — pinned either way | P0 |
| S11 | Populated, `daily.json` corrupt | Any | B3: defined behavior, and the message names `sync repair` | P0 |
| S12 | Populated, one shard body corrupt | Any | B2: that shard excluded and reported; every other machine still merges | P0 |
| S13 | Populated, `keys.json` absent/corrupt/newer | Duplicate usage across machines | Rebuilt; the pass may over-count once, never under-count; converged after one more pass | P1 |
| S14 | Populated, index promises a missing shard | Any | Counted as missing, reported, not fatal (covered — keep) | — |
| S15 | Populated, shard from a newer ccusage | Any | Skipped whole, reported (covered — keep) | — |
| S16 | Populated, orphan shard not in the index | Any | `repair` finds and indexes it; totals include it afterwards | P1 |
| S17 | Populated, shard body's machine/date ≠ its key | Any | B5: refused as an anomaly, not applied-then-forgotten | P1 |
| S18 | Populated, salt in config ≠ salt in bucket | Any | Refused before any write (covered in `salt.rs` — extend to `run`) | P1 |
| S19 | Populated, machine's clock 3h fast | Any | Refused before folding, message names the direction (now real after B1) | P0 |
| S20 | Populated, bucket set up a week ago, clock fine | Any | Runs (B1 regression) | P0 |

### 4.2 Failure-injection matrix: where the wire breaks

Each row: inject the fault at that boundary, assert the remote state, then run again cleanly and
assert convergence. "Converges" means *the totals after the second run equal a clean full
recomputation*, which is the only assertion that catches the interesting bugs.

| # | Boundary | Remote state afterwards | Next run must | Pri |
| --- | --- | --- | --- | --- |
| F1 | Before any write | Untouched | Behave as a first run | P1 |
| F2 | During shard 1 of 5 | No shards, no index | Upload all 5 | — (covered) |
| F3 | During shard 3 of 5 | 2 orphan shards, no index entry | Re-upload all 5, index all 5, no double count | P0 |
| F4 | After all shards, before index CAS | 5 orphans | Re-upload and index (covered for 1 shard; extend to n) | P0 |
| F5 | Shard PUT succeeds, response lost | Shard written | Re-PUT identical bytes; hash and totals unchanged | P0 |
| F6 | During index CAS, non-conflict error | Orphans, index at old generation | Converge; `repair` also sufficient | P0 |
| F7 | Index CAS conflicts 5× | Index holds the winner's value | Fail with the "kept changing" message; no data lost; next run succeeds | P1 |
| F8 | Index written, response lost | Index committed | Retry hits `Conflict`, re-reads, re-applies, converges (this is the one CAS-with-lost-ack case) | P0 |
| F9 | After index, before `machine.json` | Index correct, `lastSyncAt` stale | Merge unaffected — assert the rollup does not read `machine.json` | P2 |
| F10 | During prune, after 2 of 4 deletes | 2 days gone, index still names them | Next pass reports them missing, then prune finishes | P1 |
| F11 | During rollup's shard reads | `daily.json` unchanged | Re-read and converge | P1 |
| F12 | During `daily.json` CAS | Daily unchanged, shards fine | Converge | P0 |
| F13 | Daily CAS conflicts 5× | Daily holds the winner | "rollups kept changing" message; totals still correct from the winner's pass | P1 |
| F14 | After daily, before `keys.json` | Daily new, keys stale | Suppression may be missing for one pass (over-count, never under-count); next pass fixes | P0 |
| F15 | After `keys.json`, before weekly | Weekly/monthly/models one pass behind | Dashboard totals disagree between views until the next run — assert the size of the disagreement is bounded, and that the next run fixes it | P1 |
| F16 | Between weekly and monthly | Mixed generations across derived objects | Same | P1 |
| F17 | Credentials expire mid-run | Whatever had landed | Message says re-running resumes (covered for the message; add the state assertion) | P1 |
| F18 | `forget` fails between unregister and delete | Machine off the roster, shards present | Shards invisible to rollups; `repair` re-registers or `forget` re-run finishes | P1 |

### 4.3 Concurrency matrix

Two logical processes, driven by the barrier hook (deterministic) and, for the top three rows, also
by two real threads against the shared `MemoryStore` (finds what a scripted interleaving assumes
away). Every row asserts the same closing invariant: **totals equal a clean recomputation, and no
entry present locally is absent from the bucket.**

| # | Interleaving | Pri |
| --- | --- | --- |
| C1 | Two runs on one machine, empty bucket, same logs | P0 |
| C2 | Two runs on one machine, different days each (logs grew between the two folds) | P0 |
| C3 | Two runs on one machine, same day, different contents — pins which body wins and that the index/body divergence self-heals (B6) | P0 |
| C4 | Two runs on one machine, both refreshing rollups | P0 |
| C5 | Run A uploading while run B refreshes rollups — B must not record a partial upload as final | P0 |
| C6 | Two machines uploading and refreshing concurrently | P0 |
| C7 | Two machines registering for the first time at once (roster CAS, extend the existing salt-race test) | P1 |
| C8 | A crashes after shard PUT; B runs to completion; A re-runs | P0 |
| C9 | A crashes after its index update; B refreshes rollups; totals include A's day | P0 |
| C10 | `sync run` concurrent with `sync forget` for another machine | P1 |
| C11 | `sync run` concurrent with `sync prune` | P1 |
| C12 | Continuous writer starves one process through all 5 CAS attempts — assert the error, and that nothing is lost | P1 |
| C13 | Two `merge-machine` runs at once | P2 |

### 4.4 Invariants worth asserting mechanically

These belong in a shared helper (`assert_bucket_consistent`) called at the end of every scenario
above, rather than being re-asserted by hand:

1. **Conservation** — the sum of `daily.json`'s unsuppressed cells equals a full recomputation from
   every live shard.
2. **No dangling promise** — every index entry names a shard that exists, or is reported as missing.
3. **No invisible usage** — every shard object is either named by an index in the roster, or
   reported by `repair` as an orphan.
4. **Derived agreement** — weekly, monthly and models sum to the same total as daily, or are
   provably one generation behind.
5. **Suppression symmetry** — the set of suppressed cells is identical whichever machine ran the
   pass.
6. **Idempotence** — running again changes no object body except `generatedAt` fields.
7. **Monotonic safety** — no interleaving removes a cell that no machine stopped reporting.
8. **Privacy** — no object outside `dashboard/` is written to a public key; no error string contains
   a credential, a salt, or an object body.

### 4.5 Property / fuzz layer (P1)

A deterministic pseudo-random driver over a small alphabet of operations — add entries, edit a day,
delete a day, run sync on machine *m*, inject fault at boundary *b*, refresh rollups, repair — run
for a few thousand seeded sequences, asserting invariants 1–3 and 6 after every step and a clean
recomputation at the end. This is the only technique on the list that finds the interleavings nobody
thought to enumerate, and the whole merge layer is pure enough to make it cheap. Seeds are fixed, so
failures are reproducible.

### 4.6 What cannot be tested this way

The `MemoryStore` is strongly consistent and lists immediately. GCS is strongly consistent for reads
and writes of an object, but `list` after a write is not something the model exercises, and real
preconditions are enforced by a service that can also return `503` after committing. So the model
tests are necessary and not sufficient: one end-to-end run against a real bucket — two machines, a
killed process, a re-run — remains on the list, and is still blocked on `sync setup` being run
against a billing-enabled project. Nothing in this plan should be read as having verified real GCS.

### 4.7 What is built

4.0 is done: `MemoryStore` takes keyed per-operation faults (`fail_on`), lost responses
(`lose_response_on`, which writes and *then* errors), barrier hooks (`before`), per-key operation
counts, and `fork`, which copies a bucket without its derived objects so a test can rebuild the
rollups from the shards alone and compare. `sync run`'s mutating half is extracted as
`run::commit`, so the matrices drive the real write order — register, shards, index CAS,
`machine.json`, prune, rollups — without a CLI, credentials, or log files.

`sync/merge_matrix.rs` holds 39 tests, each closing with a rebuild-and-compare:

- **4.1** — S1–S8 and S10. S9 and S11–S20 stay where they are, in `rollups.rs`, `salt.rs` and
  `failures.rs`, where they are already covered.
- **4.2** — every row: F1–F18.
- **4.3** — every row: C1–C13. C1, C2 and C3 are covered twice, once by a barrier that scripts one
  interleaving and once by two real OS threads against a shared store, which may either both
  succeed or lose one run to bounded contention and nothing else.

**4.5** is built, in `sync/merge_property.rs`: a seeded driver walks 64 fixed sequences of 40 steps
over `{log, lose a day, sync, sync into a fault at one of six boundaries, refresh, repair}` for two
machines and five days, asserting after every step that nothing is promised without an object
behind it, that no total is a number no machine reported, that the incremental rollups equal a
rebuild from the shards, and that failures name neither the salt, the user id nor an object body.
Conservation is excused in exactly one state, which the driver derives rather than assumes: while a
machine's shard object is ahead of the hash its index promises — the residue of a run that died
between the two writes — an index-following pass and a body-reading pass are allowed to differ,
until that machine uploads the day again or a repair re-reads the bodies. Every sequence closes
with a repair, a refresh, and a rebuild-and-compare. Dedupe keys are unique per machine and day
there, so cross-machine suppression is left to its own tests in 4.1 and `duplicates.rs`.

The real-bucket run in 4.6 remains unbuilt: nothing here has been run against real GCS.

Two behaviours changed because these tests found them. `prune` now withdraws a day's index promise
before deleting its object, not after: the old order could leave an entry pointing at nothing, and
since a rollup pass skips a day whose hash it already holds, the gap was never even reported — an
interrupted prune now leaves an orphan object instead, which is a state the merge already
classifies. And `repair` now sweeps a registered machine whose index still promises days it has no
objects for, so a promise orphaned out of band is cleared rather than left for a run that may never
come.

## 5. Suggested order

1. B1 (done), B2, B4, B7 — the four ways the merge silently or needlessly loses usage today.
2. Test-double work in 4.0, then the P0 rows of 4.1/4.2/4.3 with `assert_bucket_consistent`.
3. B3, B5, B6 (the lockfile), B8.
4. The property layer, then the real-bucket run.
