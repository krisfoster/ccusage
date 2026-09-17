# Cloud Sync Setup

Cloud sync copies your local usage totals into an object storage bucket you own,
so several machines can contribute to one view of your spend.

`ccusage sync setup`, `ccusage sync run`, `ccusage sync status`, and
`ccusage sync doctor` work today. `run` uploads Claude Code usage; the other
agents and the dashboard arrive in later releases.

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

Uploaded days hold token counts, costs, and 15-minute activity buckets. Project
paths are hashed with your bucket's salt before upload, and prompts, file
contents, and file paths are never uploaded.

## Privacy

Buckets ccusage creates use uniform bucket-level access and grant nothing to
`allUsers`. When the dashboard ships, only its assets become public; usage data
stays private and is shared through time-limited links rather than by opening the
bucket. Setup and `doctor` both refuse to continue against a bucket that is
readable by the world.

## See also

- [Configuration Files](/guide/config-files)
- [Environment Variables](/guide/environment-variables)
- [JSON Output](/guide/json-output)
