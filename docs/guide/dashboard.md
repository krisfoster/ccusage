# Dashboard

`ccusage sync dashboard` renders everything in your sync bucket — totals, daily spend, models, machines, agents, and what the same tokens would have cost at other providers — as a single page.

The page and the data are separate things, and that is the point: the page can be world-readable while the numbers on it stay private.

## Host it locally

```bash
ccusage sync dashboard
```

This reads the rollups with your own credentials, serves them from `127.0.0.1`, and prints the URL. Nothing is uploaded, nothing is made public, and no token leaves the process. Add `--open` to launch a browser.

Run `ccusage sync run` first — the dashboard renders the rollups in the bucket, not your local logs.

## Publish it

```bash
ccusage sync dashboard --deploy
```

This uploads the page (`index.html`, `app.js`, `styles.css`) plus the third-party price table and the model-equivalence map to `<prefix>/dashboard/` and grants `allUsers` read on **that prefix only**, through an IAM condition. Your shards and rollups are not uploaded there and stay private.

A deployed page with no link shows an explanation rather than data. That is the intended resting state.

## Share the data

```bash
ccusage sync dashboard --share --share-ttl 3600
```

This mints time-boxed signed URLs for the four rollup objects and returns a link of the form `…/dashboard/index.html#s=<encoded urls>`. The URLs ride in the location fragment, which browsers do not send to servers and which therefore stays out of access logs, proxies, and `Referer` headers.

It is still a bearer link: anyone holding it can read that data until it expires. To revoke early, deactivate the HMAC key it was signed with (`gcloud storage hmac update --deactivate`), which invalidates every link signed by that key.

Share links need an HMAC credential (`CCUSAGE_SYNC_HMAC_ACCESS_ID` / `CCUSAGE_SYNC_HMAC_SECRET`); application-default credentials cannot sign a URL locally. If you have only ADC, host locally instead.

## Access models at a glance

| Mode | Page | Data | Who can read the numbers |
| --- | --- | --- | --- |
| `ccusage sync dashboard` | loopback | loopback | you, on this machine |
| `--deploy` | public | private | nobody, until shared |
| `--deploy --share` | public | signed URLs | anyone with the link, until it expires |

## What the page shows

- Totals for the selected window, plus freshness per machine.
- Daily spend, re-bucketed into the timezone you pick — the stored data is 15-minute UTC cells, so `+05:45` and DST transitions are handled honestly rather than by rounding to a UTC day.
- Breakdowns by model, machine, and agent, and weekly and monthly tables.
- Late-edit anomalies and cross-machine duplicate suppression, so a number that looks low has a visible reason.
- A provider comparison table using the same arithmetic as [`ccusage compare`](/guide/provider-comparison) — token counts repriced at another provider's standard list rates.

## Custom domains

Serving the page from your own domain means putting a CDN or load balancer in front of the bucket (for GCS, a backend bucket behind an HTTPS load balancer). The signed rollup URLs still point at `storage.googleapis.com`, so the page must be allowed to fetch cross-origin — `ccusage sync setup` already configures the bucket's CORS rules for this.

## See also

- [Cloud Sync](/guide/cloud-sync) — setup, merge semantics, and the object layout.
- [Provider Comparison](/guide/provider-comparison) — the counterfactual pricing model and its caveats.
