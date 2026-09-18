//! Every agent on this machine, reduced to the entries a shard can hold.
//!
//! Shards are keyed by agent, so syncing a second agent is a matter of folding
//! its entries under a different name rather than a layout change. What this
//! module owns is the part that cannot be shared: each adapter's loader has its
//! own signature, and Codex does not produce [`LoadedEntry`] at all.
//!
//! One agent failing to load never fails the sync. A broken or half-written log
//! in one tool would otherwise strand every other tool's usage, and the
//! alternative — silently syncing less than the user has — is worse than a
//! warning, so failures are returned and reported rather than swallowed.

use ccusage_core::{LoadedEntry, TokenUsageRaw, calculate_cost_for_usage_at};
use ccusage_sync::FoldEntry;

use crate::{
    adapter,
    cli::{AgentReportKind, CostMode, SharedArgs},
    pricing::PricingMap,
};

/// One agent's entries, under the name the shard layout uses for it.
pub(crate) struct AgentEntries {
    pub(crate) agent: &'static str,
    pub(crate) entries: Vec<FoldEntry>,
}

/// An agent whose logs could not be read. Reported, never fatal.
pub(crate) struct SourceFailure {
    pub(crate) agent: &'static str,
    pub(crate) detail: String,
}

pub(crate) struct Sources {
    pub(crate) agents: Vec<AgentEntries>,
    pub(crate) failures: Vec<SourceFailure>,
}

impl Sources {
    /// Agents that had at least one entry, for the run summary.
    pub(crate) fn named(&self) -> Vec<&'static str> {
        self.agents.iter().map(|agent| agent.agent).collect()
    }
}

/// Loads every agent this build knows about.
///
/// The adapters are read serially rather than in parallel: sync runs
/// unattended and rarely, and the wall-clock saving is not worth another
/// thread pool between the user's logs and their bucket.
pub(crate) fn load_all(shared: &SharedArgs, pricing: &PricingMap) -> Sources {
    let mut agents = Vec::new();
    let mut failures = Vec::new();
    for agent in SYNCED_AGENTS {
        match load_agent(agent, shared, pricing) {
            Ok(entries) if entries.is_empty() => {}
            Ok(entries) => agents.push(AgentEntries { agent, entries }),
            Err(error) => failures.push(SourceFailure {
                agent,
                detail: error.to_string(),
            }),
        }
    }
    Sources { agents, failures }
}

/// The agents this build can fold into shards.
///
/// Held separately from the dispatch below so a test can check it against
/// [`ccusage_core::BUILT_IN_AGENT_NAMES`] without reading anyone's logs: an
/// adapter added to ccusage and forgotten here would sync as no usage at all.
const SYNCED_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "amp",
    "droid",
    "codebuff",
    "hermes",
    "pi",
    "goose",
    "openclaw",
    "kilo",
    "copilot",
    "gemini",
    "antigravity",
    "kimi",
    "qwen",
    "grok",
    "zcode",
];

/// One agent's entries, dispatched by the name the shard layout uses.
///
/// An unrecognized name is an error rather than an empty result: syncing
/// nothing under an agent's name is indistinguishable, in the bucket, from
/// having used it and spent nothing.
fn load_agent(
    agent: &str,
    shared: &SharedArgs,
    pricing: &PricingMap,
) -> crate::Result<Vec<FoldEntry>> {
    let entries = match agent {
        "claude" => adapter::claude::load_entries(shared, None)?,
        "codex" => return load_codex(shared, pricing),
        "opencode" => adapter::opencode::load_entries(shared, AgentReportKind::Session)?,
        "amp" => adapter::amp::load_entries(shared, pricing)?,
        "droid" => adapter::droid::load_entries(shared, pricing)?,
        "codebuff" => adapter::codebuff::load_entries(shared, pricing)?,
        "hermes" => adapter::hermes::load_entries(shared, pricing)?,
        "pi" => adapter::pi::load_entries(shared, None, Some(pricing))?,
        "goose" => adapter::goose::load_entries(shared, pricing)?,
        "openclaw" => adapter::openclaw::load_entries(shared, None, Some(pricing))?,
        "kilo" => adapter::kilo::load_entries(shared, pricing)?,
        "copilot" => adapter::copilot::load_entries(shared, pricing)?,
        "gemini" => adapter::gemini::load_entries(shared, pricing)?,
        "antigravity" => adapter::antigravity::load_entries(shared, pricing)?,
        "kimi" => adapter::kimi::load_entries(shared, pricing)?,
        "qwen" => adapter::qwen::load_entries(shared)?,
        "grok" => adapter::grok::load_entries(shared, pricing)?,
        "zcode" => adapter::zcode::load_entries(shared, pricing)?,
        other => {
            return Err(crate::cli_error(format!(
                "{other} has no sync loader; its usage would be missing from the bucket"
            )));
        }
    };
    Ok(to_fold_entries(&entries))
}

/// Entries as the adapters produce them, reduced to what a shard may hold.
pub(crate) fn to_fold_entries(entries: &[LoadedEntry]) -> Vec<FoldEntry> {
    entries
        .iter()
        .map(|entry| FoldEntry {
            timestamp_ms: entry.timestamp.as_millis(),
            model: entry.model.clone().unwrap_or_else(|| "unknown".to_string()),
            input_tokens: entry.data.message.usage.input_tokens,
            output_tokens: entry.data.message.usage.output_tokens,
            cache_write_tokens: entry.data.message.usage.cache_creation_input_tokens,
            cache_read_tokens: entry.data.message.usage.cache_read_input_tokens,
            cost: entry.cost,
            session_id: entry.session_id.to_string(),
            project_path: entry.project_path.to_string(),
            message_id: entry.data.message.id.clone(),
            request_id: entry.data.request_id.clone(),
        })
        .collect()
}

/// Codex records token-usage events rather than message entries, so it is
/// mapped here instead of going through [`to_fold_entries`].
///
/// Its events carry no message or request ID, so dedupe across machines falls
/// back to the session and timestamp the fold derives, and its reasoning
/// output is already part of `output_tokens`.
fn load_codex(shared: &SharedArgs, pricing: &PricingMap) -> crate::Result<Vec<FoldEntry>> {
    let (events, _detected) = adapter::codex::load_codex_events_with_detection(shared)?;
    Ok(codex_fold_entries(&events, pricing))
}

fn codex_fold_entries(
    events: &[adapter::codex::CodexTokenUsageEvent],
    pricing: &PricingMap,
) -> Vec<FoldEntry> {
    events
        .iter()
        .filter_map(|event| {
            let timestamp = ccusage_core::parse_ts_timestamp(&event.timestamp)?;
            let usage = TokenUsageRaw {
                input_tokens: event.input_tokens,
                output_tokens: event.output_tokens,
                cache_creation_input_tokens: event.cache_creation_tokens,
                cache_read_input_tokens: event.cached_input_tokens,
                speed: None,
                cache_creation: None,
            };
            Some(FoldEntry {
                timestamp_ms: timestamp.as_millis(),
                model: event.model.clone().unwrap_or_else(|| "unknown".to_string()),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_write_tokens: usage.cache_creation_input_tokens,
                cache_read_tokens: usage.cache_read_input_tokens,
                cost: calculate_cost_for_usage_at(
                    event.model.as_deref(),
                    usage,
                    None,
                    Some(timestamp),
                    CostMode::Auto,
                    Some(pricing),
                ),
                session_id: event.session_id.clone(),
                // Codex sessions are not filed under a project directory.
                project_path: String::new(),
                message_id: None,
                request_id: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build that grows an adapter and forgets the sync dispatch would sync
    /// that agent's usage as nothing at all, silently.
    #[test]
    fn every_built_in_agent_has_a_sync_loader() {
        let mut synced = SYNCED_AGENTS.to_vec();
        let mut built_in = ccusage_core::BUILT_IN_AGENT_NAMES.to_vec();
        synced.sort_unstable();
        built_in.sort_unstable();

        assert_eq!(synced, built_in);
    }

    #[test]
    fn an_unknown_agent_is_refused_rather_than_silently_skipped() {
        let error = load_agent(
            "not-an-agent",
            &SharedArgs::with_defaults(),
            &PricingMap::default(),
        )
        .expect_err("unknown agent");

        assert!(error.to_string().contains("has no sync loader"));
    }

    /// Codex reports cached input separately from cache creation, and the
    /// shard's two cache columns have to line up with the rest of the agents.
    #[test]
    fn a_codex_event_keeps_its_tokens_session_and_cache_split() {
        let event = adapter::codex::CodexTokenUsageEvent {
            session_id: "session-a".to_string(),
            timestamp: "2026-09-17T18:12:03.000Z".to_string(),
            model: Some("gpt-5-codex".to_string()),
            input_tokens: 10,
            cached_input_tokens: 4,
            cache_creation_tokens: 2,
            output_tokens: 7,
            reasoning_output_tokens: 3,
            total_tokens: 23,
            is_fallback_model: false,
            service_tier: None,
        };

        let folded = codex_fold_entries(std::slice::from_ref(&event), &PricingMap::default());

        assert_eq!(folded.len(), 1);
        let entry = &folded[0];
        assert_eq!(entry.model, "gpt-5-codex");
        assert_eq!(entry.session_id, "session-a");
        assert_eq!(entry.input_tokens, 10);
        assert_eq!(entry.output_tokens, 7);
        assert_eq!(entry.cache_write_tokens, 2);
        assert_eq!(entry.cache_read_tokens, 4);
        assert_eq!(entry.timestamp_ms, 1_789_668_723_000);
    }

    /// An event with an unparseable timestamp has no day to belong to, and
    /// guessing one would move the user's history on every sync.
    #[test]
    fn a_codex_event_without_a_usable_timestamp_is_dropped() {
        let event = adapter::codex::CodexTokenUsageEvent {
            session_id: "session-a".to_string(),
            timestamp: "not-a-timestamp".to_string(),
            model: None,
            input_tokens: 1,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 1,
            reasoning_output_tokens: 0,
            total_tokens: 2,
            is_fallback_model: false,
            service_tier: None,
        };

        assert!(codex_fold_entries(&[event], &PricingMap::default()).is_empty());
    }
}
