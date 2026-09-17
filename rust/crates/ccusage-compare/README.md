# ccusage-compare

Counterfactual spend: what the tokens you already sent would have cost on another provider.

## Owns

- `equivalence.rs` — `model-equivalence.json`, the curated map of which model each provider offers
  at each capability tier, plus the loader and the whole-file override.
- `counterfactual.rs` — `compare`/`compare_all` over `ModelUsage`, and the `Caveat` values that
  say when a number is approximate or a row was dropped.

## Why the map is a separate file

Nobody publishes an "our model is their model" table, so the pairing is editorial. Keeping it in
its own dated file means a user who disagrees replaces it wholesale, and the licensed price data
it is compared against stays untouched (see DR-10 in `specs/cloud-sync-and-dashboard.md`).

## Why the crate holds no prices

Rates arrive through the `RateSource` trait, implemented in the binary over the embedded LiteLLM
and models.dev tables. That keeps this crate's tests arithmetic — a worked example with rates
written out by hand — instead of assertions that move whenever a provider changes a price.

## Why excluded rows leave both sides

A model with no equivalent, or an equivalent with no published price, is removed from the
counterfactual *and* from the actual it is compared against, and its spend is reported separately
as `excludedCost`. Leaving it in the actual alone would invent a saving.
