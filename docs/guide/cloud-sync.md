# Cloud Sync Setup

Cloud sync copies your local usage totals into an object storage bucket you own,
so several machines can contribute to one view of your spend.

`run` uploads usage from every agent ccusage supports — Claude Code, Codex,
Gemini, Copilot, OpenCode, and the rest — stored one shard per agent per UTC
day. An agent whose logs cannot be read is reported at the end of the run and
skipped; the other agents still sync.

See [Dashboard](/guide/dashboard) for reading the result back.

::: warning Google Cloud Storage only
`--provider gcs` is the only provider so far. The storage layer is
provider-neutral, so S3-compatible providers can be added without changing the
data layout or your bucket.
:::

## What gets uploaded

Only the numbers ccusage already shows you: per-model token counts and costs,
bucketed into 15-minute UTC windows, plus the machine and session identifiers
needed to merge them. Prompts, responses, and file contents from your agent logs
are never read into the bucket.

## Setup

```bash
ccusage sync setup
```

Setup authenticates, picks a project, creates a bucket if you do not already have
one, and writes the result to your [configuration file](/guide/config-files):

```json
{
	"$schema": "https://ccusage.com/config-schema.json",
	"sync": {
		"provider": "gcs",
		"projectId": "my-project",
		"bucket": "ccusage-9f3a1c2b4405",
		"location": "US",
		"prefix": "ccusage/v1",
		"machineId": "a1b2c3d4",
		"userId": "u-1234",
		"salt": "00112233445566778899aabbccddeeff"
	}
}
```

Credentials are never written to the configuration file. `salt` is not a
credential — it is the value that makes the bucket's hashed project names
unguessable — but every machine writing to the bucket must use the same one, so
it is created once and kept in the bucket as well as in your configuration.

### Authentication

Setup tries these in order and stops at the first one that works:

1. `CCUSAGE_SYNC_ACCESS_TOKEN`
2. `CCUSAGE_SYNC_HMAC_ACCESS_ID` and `CCUSAGE_SYNC_HMAC_SECRET`
3. `GOOGLE_APPLICATION_CREDENTIALS`
4. Application Default Credentials from `gcloud`
5. `gcloud auth print-access-token`
6. The GCE metadata server

If none are available and the terminal is interactive, setup offers to run
`gcloud auth application-default login` for you and opens your browser; pressing
Enter accepts. With `--non-interactive` it fails instead of asking, which is what
you want in CI.

`--auth adc` ignores the environment credentials, and `--auth hmac` accepts only
an HMAC pair — use it when the machine must not authenticate as whoever last ran
`gcloud`.

### Choosing a project and bucket

The project comes from `--project`, then `CLOUDSDK_CORE_PROJECT`,
`GOOGLE_CLOUD_PROJECT`, or `GCLOUD_PROJECT`, and otherwise from the projects your
credential can see. A single project is used automatically; several prompt you to
choose, or, with `--non-interactive`, fail and list the IDs.

Pass `--bucket` to use a bucket you already have. Otherwise setup asks for a
name, suggesting `ccusage-<random>` — press Enter to take the suggestion, or type
your own. Bucket names are globally unique across Google Cloud, so a name
someone else already holds is rejected. With `--non-interactive` the suggested
name is used without asking. Re-running setup reuses the configured bucket;
pointing an already configured machine at a different bucket needs `--recreate`,
so a typo cannot quietly strand your history.

### A second machine

Run setup on the second machine with the bucket from the first:

```bash
ccusage sync setup --bucket ccusage-9f3a1c2b4405
```

It adopts the user identity and hash salt recorded in the bucket and keeps its
own machine identity, so each machine writes its own objects and nothing is
overwritten. If that machine's configuration already names a different salt,
setup stops: hashing with two salts would make the same activity look like two
different sets of usage and double your totals.

## Checking the configuration

`ccusage sync status` prints the resolved target without touching the network:

```bash
ccusage sync status
```

```bash
ccusage sync status --json
```

```json
{
	"configured": true,
	"provider": "gcs",
	"projectId": "my-project",
	"bucket": "ccusage-9f3a1c2b4405",
	"location": "US",
	"prefix": "ccusage/v1",
	"machineId": "a1b2c3d4",
	"machineLabel": "laptop",
	"userId": "u-1234",
	"lastSyncAt": null
}
```

`ccusage sync doctor` does the checks that need the bucket: that your credential
can write and read back an object, that the bucket honors compare-and-swap
preconditions (without them, two machines syncing at once would overwrite each
other), that this machine's clock is close enough to the bucket's for usage to
land in the right 15-minute window, and that nothing has made the bucket
world-readable. It exits non-zero if any check fails.

```bash
ccusage sync doctor --json
```

## Uploading

```bash
ccusage sync run
ccusage sync run --dry-run
```

`run` folds your local usage into one object per UTC day and uploads only the
days whose contents changed, so a run that finds nothing new writes nothing.
Days are stored under this machine's own prefix, so two machines never overwrite
each other's data and a day you sync from a laptop stays intact when a desktop
syncs the same day.

### Settled days

Two days after a UTC day ends, its uploaded data is marked settled and is not
expected to change again. If your local logs later disagree with a settled day,
`run` still uploads the correction — your machine's logs are the authority for
its own usage — but warns you:

```
Warning: 1 finalized day(s) changed and were rewritten: 2026-09-10.
```

The correction is recorded so the dashboard can flag the day, which matters if
you exported or quoted a total before it moved. A run that reports this
repeatedly for the same day usually means a clock problem or a log being
rewritten after the fact.

### Machines that see the same logs

If two machines read the same usage logs — a synced home directory, a restored
backup — both upload them, and the totals count them once. Each 15-minute
activity bucket carries a set of per-message fingerprints, and when one
machine's set is wholly contained in another's, one copy is left out of the
totals rather than added to them.

An overlap that is only partial is never suppressed: the two machines really did
see some of the same messages and some different ones, so both are counted and
the bucket is flagged for the dashboard instead. Suppression is recomputed from
scratch on every sync, so removing a machine restores whatever it was masking.

Uploaded days hold token counts, costs, and 15-minute activity buckets. Project
paths are hashed with your bucket's salt before upload, and prompts, file
contents, and file paths are never uploaded.

### Deleting old days

Nothing is ever deleted unless you ask for it:

```bash
ccusage sync run --prune 365
```

`--prune <days>` deletes uploaded days older than that many days, from every
machine in the bucket, and rewrites the totals so the deleted days no longer
count. Your local logs are untouched. Pair it with `--dry-run` to see which
days would go before any of them do.

## When a sync is interrupted

Every step is written in an order that makes a half-finished sync safe: a day's
data is uploaded before the index that names it, and the index before the totals
derived from it. A sync that is killed, loses its connection, or hits an expired
credential leaves the bucket readable and correct — just missing the part it
never wrote — and re-running picks up from there without duplicating anything.

Two cases stop a run before it writes:

- **A clock that is more than an hour from the bucket's.** Usage would be filed
  under the wrong days, and nothing downstream could tell. Fix the system clock
  and run again; `ccusage sync doctor` reports the skew.
- **Another machine writing the same object at the same time.** The run reports
  the clash rather than overwriting the other machine's work.

`run`, `repair`, `forget`, and `merge-machine` all exit non-zero on failure and
print what to do next.

## Maintenance

These commands are for the rare occasions when the bucket's bookkeeping and its
actual contents disagree, or when a machine is retired.

### Rebuilding from the data

```bash
ccusage sync repair
ccusage sync repair --dry-run
```

`repair` ignores the bucket's indexes and totals and rebuilds them by listing
what is actually there: machines missing from the roster are re-registered,
missing or corrupt indexes are rewritten, and the rollups the dashboard reads
are recomputed. It never deletes usage data, so it is safe to run whenever a
total looks wrong.

### Retiring a machine

```bash
ccusage sync forget old-laptop
ccusage sync forget old-laptop --yes
```

`forget` deletes everything a machine uploaded and removes it from the bucket's
roster, then rewrites the totals without it. It asks for confirmation first
unless you pass `--yes`. This is irreversible: the deleted days only come back
if that machine still has the local logs and syncs again.

### One machine that reinstalled

A machine that is set up again gets a new machine identity, so its history
appears twice — once under each identity. Merge them:

```bash
ccusage sync merge-machine <old-id> <new-id>
```

The old machine's days are moved under the new identity, the old identity is
removed, and the totals are rewritten. If both identities hold data for the same
day, the merge is refused rather than picking a winner — decide which copy you
want, `forget` the other machine, and merge again.

## Privacy

Buckets ccusage creates use uniform bucket-level access and grant nothing to
`allUsers`. Setup and `doctor` both refuse to continue against a bucket that is
readable by the world.

Publishing a dashboard does not change that: the page is uploaded to a separate
`<your-bucket>-dashboard` bucket, and that bucket is the only one made public.
Usage data stays in the private bucket and is shared, if at all, through
time-limited signed links.

## See also

- [Configuration Files](/guide/config-files)
- [Environment Variables](/guide/environment-variables)
- [JSON Output](/guide/json-output)
