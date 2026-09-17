//! Which model another provider would have done the same work with.
//!
//! This map is editorial, not data: nobody publishes an official "our model X
//! is their model Y" table, and any comparison built on top of it is only as
//! honest as the pairing. It therefore ships as its own file with a date on
//! it, separate from the licensed price tables, and can be replaced wholesale
//! by the user.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The map compiled into the binary.
const EMBEDDED: &str = include_str!("model-equivalence.json");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    pub label: String,
}

/// A class of model — the level of capability someone would swap like for
/// like, rather than the exact model.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tier {
    pub id: String,
    pub label: String,
    /// Substrings of a used model's name that put it in this tier.
    pub matches: Vec<String>,
    /// The model each provider offers at this tier.
    pub models: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EquivalenceMap {
    pub schema: u32,
    pub updated: String,
    #[serde(default)]
    pub note: String,
    pub providers: Vec<Provider>,
    pub tiers: Vec<Tier>,
}

impl EquivalenceMap {
    /// The curated map this build shipped with.
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("the embedded equivalence map is valid")
    }

    /// A user-supplied map, replacing the embedded one entirely rather than
    /// merging: a partial override would silently keep pairings the user was
    /// trying to disagree with.
    pub fn parse(json: &str) -> Result<Self, String> {
        let map: Self = serde_json::from_str(json)
            .map_err(|error| format!("invalid equivalence map: {error}"))?;
        if map.schema != 1 {
            return Err(format!(
                "equivalence map schema {} is not supported by this build",
                map.schema
            ));
        }
        Ok(map)
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    /// The tier a used model belongs to.
    ///
    /// The longest matching substring wins, so `claude-haiku-4-5` lands in
    /// `fast` even though a shorter `claude` pattern would also match.
    pub fn tier_of(&self, model: &str) -> Option<&Tier> {
        let lowered = model.to_ascii_lowercase();
        self.tiers
            .iter()
            .filter_map(|tier| {
                tier.matches
                    .iter()
                    .filter(|pattern| lowered.contains(&pattern.to_ascii_lowercase()))
                    .map(String::len)
                    .max()
                    .map(|length| (length, tier))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, tier)| tier)
    }

    /// The model `provider` would have used instead of `model`.
    pub fn equivalent(&self, model: &str, provider: &str) -> Option<&str> {
        self.tier_of(model)
            .and_then(|tier| tier.models.get(provider))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_map_names_a_model_for_every_provider_in_every_tier() {
        let map = EquivalenceMap::embedded();

        for tier in &map.tiers {
            for provider in &map.providers {
                assert!(
                    tier.models.contains_key(&provider.id),
                    "tier {} has no {} model",
                    tier.id,
                    provider.id
                );
            }
        }
    }

    /// A shorter pattern must not capture a model a more specific one claims,
    /// or every Claude model would be compared as though it were an Opus.
    #[test]
    fn the_most_specific_pattern_decides_the_tier() {
        let map = EquivalenceMap::embedded();

        assert_eq!(map.tier_of("claude-opus-4-1").expect("tier").id, "frontier");
        assert_eq!(
            map.tier_of("claude-sonnet-4-5").expect("tier").id,
            "workhorse"
        );
        assert_eq!(map.tier_of("claude-haiku-4-5").expect("tier").id, "fast");
    }

    #[test]
    fn a_model_nobody_mapped_has_no_tier_rather_than_a_guessed_one() {
        let map = EquivalenceMap::embedded();

        assert!(map.tier_of("some-local-llama").is_none());
    }

    #[test]
    fn a_used_model_resolves_to_the_target_providers_model() {
        let map = EquivalenceMap::embedded();

        assert_eq!(map.equivalent("claude-sonnet-4-5", "zai"), Some("glm-4.6"));
        assert_eq!(
            map.equivalent("claude-opus-4-1", "google"),
            Some("gemini-2.5-pro")
        );
        assert_eq!(map.equivalent("claude-sonnet-4-5", "nobody"), None);
    }

    #[test]
    fn an_override_replaces_the_embedded_map_and_is_version_checked() {
        let replacement = r#"{
			"schema": 1,
			"updated": "2026-01-01",
			"providers": [{ "id": "zai", "label": "z.ai" }],
			"tiers": [{
				"id": "everything",
				"label": "Everything",
				"matches": ["claude"],
				"models": { "zai": "glm-5.3" }
			}]
		}"#;

        let map = EquivalenceMap::parse(replacement).expect("override");

        assert_eq!(map.equivalent("claude-haiku-4-5", "zai"), Some("glm-5.3"));
        assert!(
            EquivalenceMap::parse(&replacement.replace("\"schema\": 1", "\"schema\": 99"))
                .expect_err("future schema")
                .contains("not supported")
        );
    }
}
