//! What the same tokens would have cost somewhere else.
//!
//! The arithmetic is trivial; the honesty is not. Three things can make the
//! answer wrong, and each is carried out of here as a note rather than folded
//! into the number: a model nobody mapped, a mapped model nobody prices, and a
//! provider that does not charge for writing to its cache the way the source
//! provider does.

use serde::Serialize;

use crate::equivalence::EquivalenceMap;

/// Per-million-token rates, as every published price table states them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    /// `None` when the provider publishes no cache-write rate, which is not
    /// the same as publishing zero.
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
}

/// Where the rates for a model come from. Implemented over the binary's
/// pricing tables, so this crate needs no price data of its own.
pub trait RateSource {
    fn rates(&self, model: &str) -> Option<Rates>;
}

/// One model's usage, as the rollups record it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    /// What it actually cost, as already computed by the report.
    pub cost: f64,
}

/// Why a row's counterfactual cost is missing or approximate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Caveat {
    /// No tier claims this model, so there is nothing to compare it with.
    Unmapped { model: String },
    /// A tier names an equivalent, but no price table has it.
    Unpriced { model: String, equivalent: String },
    /// The target publishes no cache-write rate, so cached writes are charged
    /// at its input rate — the assumption that makes the comparison possible
    /// and the one most likely to flatter the target.
    CacheWriteAtInputRate { equivalent: String },
}

impl Caveat {
    pub fn to_text(&self) -> String {
        match self {
            Self::Unmapped { model } => {
                format!("{model} has no equivalent on this provider, so its spend is excluded")
            }
            Self::Unpriced { model, equivalent } => {
                format!(
                    "{equivalent} (the equivalent of {model}) has no published price, so its spend is excluded"
                )
            }
            Self::CacheWriteAtInputRate { equivalent } => {
                format!(
                    "{equivalent} publishes no cache-write price, so cache writes are charged at its input rate"
                )
            }
        }
    }
}

/// One used model against one target provider.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub model: String,
    pub equivalent: Option<String>,
    pub tier: Option<String>,
    pub actual_cost: f64,
    /// `None` when the row could not be priced; the caveats say why.
    pub counterfactual_cost: Option<f64>,
}

/// A whole provider's answer to "what if I had used them instead".
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub provider: String,
    pub provider_label: String,
    /// Actual spend on the rows that could be compared. Deliberately not the
    /// total spend: comparing a partial counterfactual against a full actual
    /// would invent a saving out of the rows that were dropped.
    pub actual_cost: f64,
    pub counterfactual_cost: f64,
    /// Actual minus counterfactual: positive means the target is cheaper.
    pub saving: f64,
    /// `None` when there was no comparable spend to be a percentage of.
    pub saving_percent: Option<f64>,
    /// Spend that no row could account for, so a reader can see how much of
    /// the bill the comparison is silent about.
    pub excluded_cost: f64,
    pub rows: Vec<Row>,
    pub caveats: Vec<Caveat>,
}

impl Comparison {
    /// How many of the used models this provider could actually be priced
    /// against.
    pub fn priced_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.counterfactual_cost.is_some())
            .count()
    }
}

const PER_MILLION: f64 = 1_000_000.0;

fn charge(tokens: u64, rate: f64) -> f64 {
    (tokens as f64) * rate / PER_MILLION
}

/// Prices `usage` as though every model had been the target provider's
/// equivalent.
pub fn compare(
    usage: &[ModelUsage],
    provider: &str,
    map: &EquivalenceMap,
    rates: &dyn RateSource,
) -> Result<Comparison, String> {
    let known = map
        .provider(provider)
        .ok_or_else(|| format!("unknown provider '{provider}'"))?;
    let mut comparison = Comparison {
        provider: known.id.clone(),
        provider_label: known.label.clone(),
        actual_cost: 0.0,
        counterfactual_cost: 0.0,
        saving: 0.0,
        saving_percent: None,
        excluded_cost: 0.0,
        rows: Vec::new(),
        caveats: Vec::new(),
    };
    for entry in usage {
        let tier = map.tier_of(&entry.model);
        let equivalent = tier.and_then(|tier| tier.models.get(provider));
        let Some(equivalent) = equivalent else {
            comparison.excluded_cost += entry.cost;
            comparison.caveats.push(Caveat::Unmapped {
                model: entry.model.clone(),
            });
            comparison.rows.push(Row {
                model: entry.model.clone(),
                equivalent: None,
                tier: tier.map(|tier| tier.id.clone()),
                actual_cost: entry.cost,
                counterfactual_cost: None,
            });
            continue;
        };
        let Some(target) = rates.rates(equivalent) else {
            comparison.excluded_cost += entry.cost;
            comparison.caveats.push(Caveat::Unpriced {
                model: entry.model.clone(),
                equivalent: equivalent.clone(),
            });
            comparison.rows.push(Row {
                model: entry.model.clone(),
                equivalent: Some(equivalent.clone()),
                tier: tier.map(|tier| tier.id.clone()),
                actual_cost: entry.cost,
                counterfactual_cost: None,
            });
            continue;
        };
        let cache_write_rate = match target.cache_write {
            Some(rate) => rate,
            None => {
                if entry.cache_write_tokens > 0 {
                    let caveat = Caveat::CacheWriteAtInputRate {
                        equivalent: equivalent.clone(),
                    };
                    if !comparison.caveats.contains(&caveat) {
                        comparison.caveats.push(caveat);
                    }
                }
                target.input
            }
        };
        let cost = charge(entry.input_tokens, target.input)
            + charge(entry.output_tokens, target.output)
            + charge(entry.cache_write_tokens, cache_write_rate)
            + charge(
                entry.cache_read_tokens,
                target.cache_read.unwrap_or(target.input),
            );
        comparison.actual_cost += entry.cost;
        comparison.counterfactual_cost += cost;
        comparison.rows.push(Row {
            model: entry.model.clone(),
            equivalent: Some(equivalent.clone()),
            tier: tier.map(|tier| tier.id.clone()),
            actual_cost: entry.cost,
            counterfactual_cost: Some(cost),
        });
    }
    comparison.saving = comparison.actual_cost - comparison.counterfactual_cost;
    if comparison.actual_cost > 0.0 {
        comparison.saving_percent = Some(comparison.saving / comparison.actual_cost * 100.0);
    }
    Ok(comparison)
}

/// Every provider in the map, cheapest counterfactual first.
///
/// A provider that could price nothing sorts last however small its total is:
/// zero spend because zero was comparable is not a saving.
pub fn compare_all(
    usage: &[ModelUsage],
    map: &EquivalenceMap,
    rates: &dyn RateSource,
) -> Result<Vec<Comparison>, String> {
    let mut comparisons = Vec::new();
    for provider in &map.providers {
        comparisons.push(compare(usage, &provider.id, map, rates)?);
    }
    comparisons.sort_by(|left, right| {
        left.priced_rows()
            .eq(&0)
            .cmp(&right.priced_rows().eq(&0))
            .then_with(|| {
                left.counterfactual_cost
                    .total_cmp(&right.counterfactual_cost)
            })
            .then_with(|| left.provider.cmp(&right.provider))
    });
    Ok(comparisons)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Table(BTreeMap<&'static str, Rates>);

    impl RateSource for Table {
        fn rates(&self, model: &str) -> Option<Rates> {
            self.0.get(model).copied()
        }
    }

    fn table() -> Table {
        Table(BTreeMap::from([
            (
                "claude-sonnet-4-5",
                Rates {
                    input: 3.0,
                    output: 15.0,
                    cache_write: Some(3.75),
                    cache_read: Some(0.3),
                },
            ),
            (
                "glm-4.6",
                Rates {
                    input: 0.6,
                    output: 2.2,
                    cache_write: None,
                    cache_read: Some(0.11),
                },
            ),
        ]))
    }

    fn sonnet_usage() -> Vec<ModelUsage> {
        vec![ModelUsage {
            model: "claude-sonnet-4-5".to_string(),
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_write_tokens: 200_000,
            cache_read_tokens: 4_000_000,
            // 3.00 + 7.50 + 0.75 + 1.20
            cost: 12.45,
        }]
    }

    /// The worked example from the design doc: a million in, half a million
    /// out, plus cache traffic, priced by hand against z.ai's published rates.
    #[test]
    fn a_sonnet_month_priced_as_glm_matches_the_hand_computation() {
        let map = EquivalenceMap::embedded();

        let comparison = compare(&sonnet_usage(), "zai", &map, &table()).expect("comparison");

        // 0.60 + 1.10 + 0.12 (cache write at input rate) + 0.44
        assert!(
            (comparison.counterfactual_cost - 2.26).abs() < 1e-9,
            "{comparison:?}"
        );
        assert!((comparison.saving - 10.19).abs() < 1e-9, "{comparison:?}");
        assert!(
            (comparison.saving_percent.expect("percent") - 81.847).abs() < 0.01,
            "{comparison:?}"
        );
    }

    /// Charging cache writes at the input rate is an assumption in the
    /// target's favor, so it has to be stated rather than absorbed.
    #[test]
    fn a_provider_without_a_cache_write_price_says_so_once() {
        let map = EquivalenceMap::embedded();
        let mut usage = sonnet_usage();
        usage.push(ModelUsage {
            model: "claude-sonnet-4-5".to_string(),
            cache_write_tokens: 10,
            ..ModelUsage::default()
        });

        let comparison = compare(&usage, "zai", &map, &table()).expect("comparison");

        assert_eq!(
            comparison.caveats,
            vec![Caveat::CacheWriteAtInputRate {
                equivalent: "glm-4.6".to_string()
            }]
        );
    }

    /// Dropping an unmapped model from the counterfactual while keeping its
    /// cost in the actual would manufacture a saving, so both sides drop it
    /// and the excluded spend is reported.
    #[test]
    fn a_model_with_no_equivalent_is_excluded_from_both_sides() {
        let map = EquivalenceMap::embedded();
        let mut usage = sonnet_usage();
        usage.push(ModelUsage {
            model: "some-local-llama".to_string(),
            input_tokens: 1_000_000,
            cost: 99.0,
            ..ModelUsage::default()
        });

        let comparison = compare(&usage, "zai", &map, &table()).expect("comparison");

        assert!((comparison.actual_cost - 12.45).abs() < 1e-9);
        assert!((comparison.excluded_cost - 99.0).abs() < 1e-9);
        assert!(
            comparison.caveats.contains(&Caveat::Unmapped {
                model: "some-local-llama".to_string()
            }),
            "{comparison:?}"
        );
        assert_eq!(comparison.rows[1].counterfactual_cost, None);
    }

    #[test]
    fn an_equivalent_nobody_prices_is_excluded_and_named() {
        let map = EquivalenceMap::embedded();

        let comparison = compare(&sonnet_usage(), "openai", &map, &table()).expect("comparison");

        assert_eq!(comparison.counterfactual_cost, 0.0);
        assert!((comparison.excluded_cost - 12.45).abs() < 1e-9);
        assert!(matches!(
            comparison.caveats.as_slice(),
            [Caveat::Unpriced { .. }]
        ));
    }

    #[test]
    fn comparing_every_provider_puts_the_cheapest_first() {
        let map = EquivalenceMap::embedded();

        let all = compare_all(&sonnet_usage(), &map, &table()).expect("comparisons");

        assert_eq!(all.len(), map.providers.len());
        assert_eq!(all.first().expect("cheapest").provider, "zai");
        // Everyone this table cannot price sorts behind everyone it can.
        assert_eq!(all[1].provider, "anthropic");
        assert_eq!(all[2].priced_rows(), 0);
    }

    #[test]
    fn an_unknown_provider_is_an_error_rather_than_an_empty_table() {
        let map = EquivalenceMap::embedded();

        assert!(
            compare(&sonnet_usage(), "nobody", &map, &table())
                .expect_err("unknown")
                .contains("nobody")
        );
    }
}
