# Dashboard

`ccusage sync dashboard` renders everything in your sync bucket — totals, daily spend, models, machines, agents, and what the same tokens would have cost at other providers — as a single page.

The page and the data are separate things, and that is the point: the page can be world-readable while the numbers on it stay private.

## Host it locally

```bash
ccusage sync dashboard
```

This reads the rollups with your own credentials, serves them from `127.0.0.1`, and prints the URL. Nothing is uploaded, nothing is made public, and no token leaves the process. Add `--open` to launch a browser.

The server answers `localhost`, `127.0.0.1`, and `::1` only, so a page elsewhere on the internet cannot reach it by pointing a hostname of its own at your loopback address.

Run `ccusage sync run` first — the dashboard renders the rollups in the bucket, not your local logs.

## Publish it

```bash
ccusage sync dashboard --deploy
```

This uploads the page (`index.html`, `app.js`, `styles.css`) plus the third-party price table and the model-equivalence map to a **second bucket**, `<your-bucket>-dashboard`, created on demand and made world-readable. Your shards, rollups, manifests, and salts stay in the original bucket, which stays private.

Two buckets rather than a public prefix because GCS rejects an IAM condition on an `allUsers` binding (`Conditions are not allowed on public resources`), so a bucket is the only boundary it will enforce for a public grant. The dashboard bucket holds the page and published price data and nothing else.

`--deploy` needs sharing to be enabled first (see below) and mints a share link for you, so the published page shows data on first open rather than an explanation. Add `--open` to launch it.

## Enable sharing

```bash
ccusage sync share
```

Sharing is off until you ask for it. `ccusage sync setup` configures sync and nothing else; `ccusage sync dashboard --deploy` and `--share` refuse with an error pointing here until `sync share` has run, and hosting the dashboard locally never needs it.

`ccusage sync share --disable` revokes it again: the key, the account it belongs to, and the local copy all go, while the bucket and everything synced into it are untouched. Links already handed out stop working.

## Share the data

```bash
ccusage sync dashboard --share --share-ttl 3600
```

This mints time-boxed signed URLs for the four rollup objects and returns a link of the form `…/dashboard/index.html#s=<encoded urls>`. The URLs ride in the location fragment, which browsers do not send to servers and which therefore stays out of access logs, proxies, and `Referer` headers.

It is still a bearer link: anyone holding it can read that data until it expires.

### The signing key

Signing a URL needs an HMAC credential, which application-default credentials cannot produce — so `ccusage sync share` creates one for you. It creates (or adopts) a dedicated service account, `ccusage-dashboard@<project>.iam.gserviceaccount.com`, grants it `roles/storage.objectViewer` on the data bucket and nothing else, mints one HMAC key for it, and writes that key to `~/.local/state/ccusage/sync-signer.json` with mode `0600`. The key is never written to `ccusage.json`, which is shared and committed; nothing prints the secret.

Consequences worth knowing:

- Reruns of `sync share` are idempotent: an existing key is kept and the read binding re-asserted, so no second account or key accumulates. A bucket configured before this command existed can enable sharing at any time.
- A brand-new account is not immediately visible to the APIs that must accept it, so it waits (up to three minutes, saying so) for the bucket grant and the key to go through. If it runs out of time, the account it made is kept and re-running `ccusage sync share` continues from there.
- If the project forbids creating a service account, only share links are affected: sync and the local dashboard work regardless, which is why this is a separate command rather than part of setup.
- To revoke every link at once, run `ccusage sync share --disable`.
- `ccusage sync remove` deletes the key, the service account, and the local file along with the data.
- `CCUSAGE_SYNC_HMAC_ACCESS_ID` / `CCUSAGE_SYNC_HMAC_SECRET` still work and take second place to the managed key, for anyone who prefers to manage the credential themselves.

## Access models at a glance

| Mode | Page | Data | Who can read the numbers |
| --- | --- | --- | --- |
| `ccusage sync dashboard` | loopback | loopback | you, on this machine |
| `--deploy` | public | signed URLs | anyone with the printed link, until it expires |
| `--deploy` before `ccusage sync share` | not published | private | nobody — the command errors |

## What the page shows

- Totals for the selected window, plus freshness per machine.
- Daily spend as a chart with a currency axis, gridlines, and dated ticks, re-bucketed into the timezone you pick — the stored data is 15-minute UTC cells, so `+05:45` and DST transitions are handled honestly rather than by rounding to a UTC day.
- Breakdowns by model, machine, and agent, and weekly and monthly tables.
- Late-edit anomalies and cross-machine duplicate suppression, so a number that looks low has a visible reason.
- Usage from every agent ccusage supports — Claude, Codex, Gemini, Copilot, and the rest — folded per agent, not Claude alone.
- A provider comparison table using the same arithmetic as [`ccusage compare`](/guide/provider-comparison) — token counts repriced at another provider's standard list rates.
- A published price table: every comparison model with its input, output, and cache rates per million tokens, each model linking to where its provider states the rate, and every column sortable.
- A theme switcher — System, Light, or Dark. System follows your operating system and is the default; an explicit choice is remembered in the browser's local storage for that page only, and nothing about it is uploaded.

## Custom domains

Serving the page from your own domain means putting a CDN or load balancer in front of the bucket (for GCS, a backend bucket behind an HTTPS load balancer). The signed rollup URLs still point at `storage.googleapis.com`, so the page must be allowed to fetch cross-origin — `ccusage sync setup` already configures the bucket's CORS rules for this.

## See also

- [Cloud Sync](/guide/cloud-sync) — setup, merge semantics, and the object layout.
- [Provider Comparison](/guide/provider-comparison) — the counterfactual pricing model and its caveats.
- [JSON Output](/guide/json-output) — the shape of the rollups the page reads.
