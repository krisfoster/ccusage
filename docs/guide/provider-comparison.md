# Provider Comparison

`ccusage compare` reprices the usage you already have against other model
providers: if the same tokens had gone to z.ai, DeepSeek, OpenAI, Google, xAI,
Mistral, or Moonshot, what would the bill have been?

```bash
ccusage compare
ccusage compare --provider zai
ccusage compare --since 20250101 --until 20250131
```

```
┌───────────┬──────────────────┬──────────────────────┬───────────┬───────────┐
│ Provider  │  Would have cost │  Actual (comparable) │    Saving │  Saving % │
├───────────┼──────────────────┼──────────────────────┼───────────┼───────────┤
│ DeepSeek  │            $0.81 │               $28.74 │    $27.93 │     97.2% │
├───────────┼──────────────────┼──────────────────────┼───────────┼───────────┤
│ z.ai      │            $2.86 │               $28.74 │    $25.88 │     90.0% │
└───────────┴──────────────────┴──────────────────────┴───────────┴───────────┘
```

The window, timezone, offline, and cost options are the shared ones documented
in [Command-Line Options](/guide/cli-options).

## How the comparison is built

1. Your usage is totalled per model: input, output, cache-write, and cache-read
   tokens.
2. Each model is matched to a capability tier — frontier, workhorse, or fast —
   and the tier names the model each provider would have handled that work with.
3. Those tokens are priced at the target model's published per-token rates.

The **Actual (comparable)** column is deliberately not your whole bill: a model
with no equivalent, or an equivalent with no published price, is dropped from
both sides so a partial counterfactual is never compared against a full bill.
Excluded spend is reported as `excludedCost` in the JSON output.

## Caveats

These numbers are an estimate, and the ways they can mislead are worth knowing:

- **Equivalence is editorial.** No provider publishes an "our model is their
  model" table. The shipped map is a judgement call, dated, and replaceable —
  see below.
- **Token counts are not portable.** Each provider tokenizes differently, so the
  same prompt is not the same number of tokens everywhere. Counts are reused
  as-is rather than re-estimated.
- **Cache pricing differs.** When a target publishes no cache-write or
  cache-read price, those tokens are charged at its input rate and the output
  says so.
- **List rates only.** Long-context tiers, batch rates, and negotiated discounts
  are not applied, so a provider compared against itself may not net to zero on
  long-context requests.
- **Plans are listed, not subtracted.** A flat-fee subscription buys
  rate-limited capacity rather than tokens, so plan prices are shown as their
  own rows instead of being folded into the saving.

## Using your own equivalence map

```bash
ccusage compare --equivalence ./my-map.json
```

The file replaces the built-in map entirely rather than merging with it, so a
pairing you disagree with cannot survive your override:

<!-- eslint-skip -->

```json
{
	"schema": 1,
	"updated": "2026-09-17",
	"providers": [
		{
			"id": "zai",
			"label": "z.ai",
			"pricingUrl": "https://docs.z.ai/guides/overview/pricing"
		}
	],
	"tiers": [
		{
			"id": "workhorse",
			"label": "Workhorse",
			"matches": ["claude-sonnet"],
			"models": { "zai": "glm-4.6" }
		}
	]
}
```

A model's tier is chosen by the longest matching `matches` entry, so a specific
pattern beats a general one. `pricingUrl` is optional and used by the dashboard,
which links each published rate back to the page the provider states it on.

## On the dashboard

The [dashboard](/guide/dashboard) runs the same arithmetic and shows two tables:
what each provider would have cost against your comparable spend, and the
published per-million rates behind those figures. Both sort by any column, and
the rate table carries no saving columns — the prices are the evidence, and a
saving computed from list rates invites more confidence than it has earned.

## JSON output

```bash
ccusage compare --json
```

The payload carries the window, the per-model token totals, one object per
provider with its rows and caveats, the plan rows, and `pricingBasis` stating
what the counterfactual does and does not include. See
[JSON Output](/guide/json-output) for the shared conventions.
