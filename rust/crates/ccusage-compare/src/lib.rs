//! "What would this have cost on another provider?" — the map that decides
//! which model to compare against, and the arithmetic that prices it.
//!
//! No price data and no I/O live here: the caller supplies rates through
//! `RateSource`, so the comparison is testable against hand-computed numbers
//! rather than against whichever price snapshot the binary happens to embed.

pub mod counterfactual;
pub mod equivalence;
pub mod plans;

pub use counterfactual::{
    Caveat, Comparison, ModelUsage, RateSource, Rates, Row, compare, compare_all,
};
pub use equivalence::EquivalenceMap;
pub use plans::{Plan, PlanList};
