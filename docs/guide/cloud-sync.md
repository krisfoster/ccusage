# Cloud Sync Setup

Cloud sync copies your local usage totals into an object storage bucket you own,
so several machines can contribute to one view of your spend.

This page covers setup only. Uploading (`ccusage sync run`) and the dashboard
arrive in later releases; `ccusage sync setup`, `ccusage sync status`, and
`ccusage sync doctor` work today.

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
		"prefix": "ccusage/v1"
	}
}
```

Credentials are never written to the configuration file.

### Authentication

Setup tries these in order and stops at the first one that works:

1. `CCUSAGE_SYNC_ACCESS_TOKEN`
2. `CCUSAGE_SYNC_HMAC_ACCESS_ID` and `CCUSAGE_SYNC_HMAC_SECRET`
3. `GOOGLE_APPLICATION_CREDENTIALS`
4. Application Default Credentials from `gcloud`
5. `gcloud auth print-access-token`
6. The GCE metadata server

If none are available and the terminal is interactive, setup offers to run
`gcloud auth application-default login` for you. With `--non-interactive` it
fails instead of asking, which is what you want in CI.

`--auth adc` ignores the environment credentials, and `--auth hmac` accepts only
an HMAC pair — use it when the machine must not authenticate as whoever last ran
`gcloud`.

### Choosing a project and bucket

The project comes from `--project`, then `CLOUDSDK_CORE_PROJECT`,
`GOOGLE_CLOUD_PROJECT`, or `GCLOUD_PROJECT`, and otherwise from the projects your
credential can see. A single project is used automatically; several prompt you to
choose, or, with `--non-interactive`, fail and list the IDs.

Pass `--bucket` to use a bucket you already have. Otherwise ccusage creates one
named `ccusage-<random>`. Re-running setup reuses the configured bucket; pointing
an already configured machine at a different bucket needs `--recreate`, so a typo
cannot quietly strand your history.

### A second machine

Run setup on the second machine with the bucket from the first:

```bash
ccusage sync setup --bucket ccusage-9f3a1c2b4405
```

It adopts the user identity recorded in the bucket and keeps its own machine
identity, so each machine writes its own objects and nothing is overwritten.

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
