//! Flat-fee subscription tiers.
//!
//! A plan buys rate-limited capacity, not tokens, so it cannot be folded into
//! the per-token counterfactual without inventing an exchange rate between the
//! two. It is carried separately and reported as its own rows, which is also
//! why the plan list is dated and replaceable rather than woven into the
//! comparison arithmetic.

use serde::{Deserialize, Serialize};

const EMBEDDED: &str = include_str!("plans.json");

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub provider: String,
    pub id: String,
    pub label: String,
    /// List price per month in USD.
    pub monthly_usd: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanList {
    pub schema: u32,
    pub updated: String,
    #[serde(default)]
    pub note: String,
    pub plans: Vec<Plan>,
}

impl PlanList {
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("the embedded plan list is valid")
    }

    pub fn for_provider<'a>(&'a self, provider: &'a str) -> impl Iterator<Item = &'a Plan> {
        self.plans
            .iter()
            .filter(move |plan| plan.provider == provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_plans_are_priced_and_attributed_to_a_provider() {
        let plans = PlanList::embedded();

        assert_eq!(plans.schema, 1);
        assert!(!plans.updated.is_empty());
        assert!(plans.plans.iter().all(|plan| plan.monthly_usd > 0.0));
        assert!(plans.for_provider("zai").count() > 0);
        assert_eq!(plans.for_provider("nobody").count(), 0);
    }
}
