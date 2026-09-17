//! `ccusage compare`: the tokens you already sent, priced as though another
//! provider had served them.
//!
//! The arithmetic and the model pairings live in `ccusage-compare`; this
//! module only turns local usage into that crate's input, hands it the
//! binary's price tables, and prints the answer.

use std::fs;

use ccusage_compare::{
    Comparison, EquivalenceMap, ModelUsage, PlanList, RateSource, Rates, compare_all,
};
use serde_json::json;

use crate::{
    Align, Result, SimpleTable, cli::CompareArgs, cli_error, fast::FxHashMap, format_currency,
    load_entries, pricing::PricingMap, print_json_or_jq, terminal_style, wants_json,
};

/// Published prices, per million tokens, out of the binary's own tables.
///
/// The tables store per-token rates; the comparison crate speaks in millions
/// because that is how every provider publishes, and the conversion is done
/// once here rather than in every caller.
struct EmbeddedRates {
    pricing: PricingMap,
}

const PER_MILLION: f64 = 1_000_000.0;

impl RateSource for EmbeddedRates {
    fn rates(&self, model: &str) -> Option<Rates> {
        let pricing = self.pricing.find_exact_with_fallback(model)?;
        Some(Rates {
            input: pricing.input * PER_MILLION,
            output: pricing.output * PER_MILLION,
            // An inferred cache-write rate (input × 1.25, applied when a
            // provider publishes none) would quietly charge the target for a
            // price it never set, so only an explicit one is used and the
            // comparison says when it fell back.
            cache_write: pricing
                .has_explicit_cache_creation_cost()
                .then(|| pricing.cache_creation_input_token_cost() * PER_MILLION),
            cache_read: pricing
                .cache_read_explicit
                .then_some(pricing.cache_read * PER_MILLION),
        })
    }
}

/// Collapses the loaded entries into one row per model.
///
/// Models are keyed by their own name rather than by tier, so the report can
/// show which of the user's models drives the difference, and a model the map
/// does not know still appears with its spend.
fn usage_by_model(entries: &[crate::LoadedEntry]) -> Vec<ModelUsage> {
    let mut totals: FxHashMap<String, ModelUsage> = FxHashMap::default();
    for entry in entries {
        let Some(model) = entry.model.as_deref() else {
            continue;
        };
        if model == "<synthetic>" {
            continue;
        }
        let usage = totals
            .entry(model.to_string())
            .or_insert_with(|| ModelUsage {
                model: model.to_string(),
                ..ModelUsage::default()
            });
        let raw = &entry.data.message.usage;
        usage.input_tokens += raw.input_tokens;
        usage.output_tokens += raw.output_tokens;
        usage.cache_write_tokens += raw.cache_creation_input_tokens;
        usage.cache_read_tokens += raw.cache_read_input_tokens;
        usage.cost += entry.cost;
    }
    let mut rows: Vec<ModelUsage> = totals.into_values().collect();
    rows.sort_by(|left, right| {
        right
            .cost
            .total_cmp(&left.cost)
            .then_with(|| left.model.cmp(&right.model))
    });
    rows
}

fn equivalence_map(args: &CompareArgs) -> Result<EquivalenceMap> {
    match args.equivalence.as_deref() {
        None => Ok(EquivalenceMap::embedded()),
        Some(path) => {
            let json = fs::read_to_string(path)
                .map_err(|error| cli_error(format!("cannot read {}: {error}", path.display())))?;
            EquivalenceMap::parse(&json).map_err(cli_error)
        }
    }
}

fn comparison_json(comparison: &Comparison) -> serde_json::Value {
    json!({
        "provider": comparison.provider,
        "providerLabel": comparison.provider_label,
        "actualCost": comparison.actual_cost,
        "counterfactualCost": comparison.counterfactual_cost,
        "saving": comparison.saving,
        "savingPercent": comparison.saving_percent,
        "excludedCost": comparison.excluded_cost,
        "rows": comparison.rows,
        "caveats": comparison
            .caveats
            .iter()
            .map(|caveat| json!({ "kind": caveat, "message": caveat.to_text() }))
            .collect::<Vec<_>>(),
    })
}

fn plans_json(comparisons: &[Comparison], plans: &PlanList) -> serde_json::Value {
    json!({
        "updated": plans.updated,
        "note": plans.note,
        "rows": comparisons
            .iter()
            .flat_map(|comparison| plans.for_provider(&comparison.provider))
            .map(|plan| json!({
                "provider": plan.provider,
                "id": plan.id,
                "label": plan.label,
                "monthlyUsd": plan.monthly_usd,
            }))
            .collect::<Vec<_>>(),
    })
}

fn print_table(
    comparisons: &[Comparison],
    map: &EquivalenceMap,
    shared: &crate::cli::SharedArgs,
) -> Result<()> {
    let mut table = SimpleTable::new(
        vec![
            "Provider",
            "Would have cost",
            "Actual (comparable)",
            "Saving",
            "Saving %",
        ],
        vec![
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        terminal_style(shared),
    );
    for comparison in comparisons {
        if comparison.priced_rows() == 0 {
            table.push(vec![
                comparison.provider_label.clone(),
                "no priced equivalent".to_string(),
                format_currency(comparison.excluded_cost),
                "-".to_string(),
                "-".to_string(),
            ]);
            continue;
        }
        table.push(vec![
            comparison.provider_label.clone(),
            format_currency(comparison.counterfactual_cost),
            format_currency(comparison.actual_cost),
            format_currency(comparison.saving),
            comparison
                .saving_percent
                .map(|percent| format!("{percent:.1}%"))
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }
    table.print()?;
    println!(
        "\nEquivalents from the {} model map. Use --equivalence <file> to substitute your own.",
        map.updated
    );
    println!(
        "Counterfactuals bill every token at the target's standard list rate, so long-context \
         tiers, batch rates, and subscription plans are not applied."
    );
    print_plans(comparisons, &PlanList::embedded(), shared)?;
    for caveat in comparisons
        .iter()
        .flat_map(|comparison| comparison.caveats.iter())
        .map(|caveat| caveat.to_text())
        .collect::<std::collections::BTreeSet<_>>()
    {
        println!("Note: {caveat}");
    }
    Ok(())
}

/// Plans are listed rather than compared: a monthly fee buys rate-limited
/// capacity, so subtracting it from a token bill would assume the plan covers
/// this workload, which is exactly the thing its rate limits decide.
fn print_plans(
    comparisons: &[Comparison],
    plans: &PlanList,
    shared: &crate::cli::SharedArgs,
) -> Result<()> {
    let rows: Vec<&ccusage_compare::Plan> = comparisons
        .iter()
        .flat_map(|comparison| plans.for_provider(&comparison.provider))
        .collect();
    if rows.is_empty() {
        return Ok(());
    }

    let mut table = SimpleTable::new(
        vec!["Plan", "Per month"],
        vec![Align::Left, Align::Right],
        terminal_style(shared),
    );
    for plan in rows {
        table.push(vec![plan.label.clone(), format_currency(plan.monthly_usd)]);
    }
    println!(
        "\nFlat-fee plans ({}), listed for reference — a plan buys rate-limited capacity, not tokens:",
        plans.updated
    );
    table.print()?;
    Ok(())
}

pub(crate) fn run(args: CompareArgs) -> Result<()> {
    let shared = args.shared.clone();
    let entries = load_entries(&shared, None)?;
    let usage = usage_by_model(&entries);
    if usage.is_empty() {
        eprintln!("No usage found for this window, so there is nothing to compare.");
        return Ok(());
    }
    let map = equivalence_map(&args)?;
    let rates = EmbeddedRates {
        pricing: PricingMap::load_with_overrides(
            shared.offline,
            false,
            shared.pricing_overrides.iter(),
        ),
    };
    let mut comparisons = compare_all(&usage, &map, &rates).map_err(cli_error)?;
    if let Some(provider) = args.provider.as_deref() {
        comparisons.retain(|comparison| comparison.provider == provider);
        if comparisons.is_empty() {
            let known: Vec<&str> = map
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect();
            return Err(cli_error(format!(
                "Unknown provider '{provider}'. This map knows: {}.",
                known.join(", ")
            )));
        }
    }

    if wants_json(&shared) {
        let output = json!({
            "window": { "since": shared.since, "until": shared.until },
            "equivalenceUpdated": map.updated,
            "pricingBasis": "standard list rates; long-context tiers, batch rates and subscription plans are not applied",
            "models": usage
                .iter()
                .map(|model| json!({
                    "model": model.model,
                    "inputTokens": model.input_tokens,
                    "outputTokens": model.output_tokens,
                    "cacheWriteTokens": model.cache_write_tokens,
                    "cacheReadTokens": model.cache_read_tokens,
                    "cost": model.cost,
                }))
                .collect::<Vec<_>>(),
            "comparisons": comparisons.iter().map(comparison_json).collect::<Vec<_>>(),
            "plans": plans_json(&comparisons, &PlanList::embedded()),
        });
        return print_json_or_jq(output, shared.jq.as_deref(), shared.no_cost);
    }
    print_table(&comparisons, &map, &shared)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ccusage_compare::compare;

    use super::*;
    use crate::{LoadedEntry, UsageEntry, UsageMessage, types::TokenUsageRaw};

    fn entry(model: &str, input_tokens: u64, cost: f64) -> LoadedEntry {
        LoadedEntry {
            data: UsageEntry {
                session_id: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                version: None,
                message: UsageMessage {
                    usage: TokenUsageRaw {
                        input_tokens,
                        output_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        speed: None,
                        cache_creation: None,
                    },
                    model: Some(model.to_string()),
                    id: None,
                },
                cost_usd: None,
                request_id: None,
                is_api_error_message: None,
                is_sidechain: None,
            },
            timestamp: crate::TimestampMs::UNIX_EPOCH,
            date: "2026-01-01".to_string(),
            project: Arc::from("p"),
            session_id: Arc::from("s"),
            project_path: Arc::from("/p"),
            cost,
            extra_total_tokens: 0,
            credits: None,
            message_count: None,
            model: Some(model.to_string()),
            usage_limit_reset_time: None,
            missing_pricing_model: None,
        }
    }

    fn rates() -> EmbeddedRates {
        EmbeddedRates {
            pricing: PricingMap::load_with_overrides(
                true,
                false,
                std::iter::empty::<(&String, &ccusage_cli::PricingOverride)>(),
            ),
        }
    }

    /// The comparison is only worth printing if the shipped map and the
    /// shipped price tables agree on model names.
    #[test]
    fn every_equivalent_in_the_shipped_map_has_a_shipped_price() {
        let map = EquivalenceMap::embedded();
        let rates = rates();

        let missing: Vec<String> = map
            .tiers
            .iter()
            .flat_map(|tier| tier.models.values())
            .filter(|model| rates.rates(model).is_none())
            .cloned()
            .collect();

        assert!(missing.is_empty(), "unpriced equivalents: {missing:?}");
    }

    /// The comparison people will actually run, against the prices this build
    /// ships rather than hand-written ones.
    #[test]
    fn a_sonnet_workload_prices_out_cheaper_on_zai() {
        let usage = vec![ModelUsage {
            model: "claude-sonnet-4-5".to_string(),
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_write_tokens: 200_000,
            cache_read_tokens: 4_000_000,
            cost: 12.45,
        }];

        let comparison =
            compare(&usage, "zai", &EquivalenceMap::embedded(), &rates()).expect("comparison");

        assert!(comparison.counterfactual_cost < comparison.actual_cost);
        assert!(comparison.saving > 9.0, "{comparison:?}");
        // z.ai publishes its own cache rates, so nothing had to be assumed.
        assert!(comparison.caveats.is_empty(), "{comparison:?}");
        assert_eq!(comparison.excluded_cost, 0.0);
    }

    #[test]
    fn entries_are_totalled_per_model_and_ordered_by_spend() {
        let entries = vec![
            entry("claude-haiku-4-5", 10, 1.0),
            entry("claude-sonnet-4-5", 20, 5.0),
            entry("claude-haiku-4-5", 30, 2.0),
        ];

        let usage = usage_by_model(&entries);

        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].model, "claude-sonnet-4-5");
        assert_eq!(usage[1].input_tokens, 40);
        assert_eq!(usage[1].cost, 3.0);
    }
}
