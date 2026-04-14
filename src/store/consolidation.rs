//! Memory consolidation: types, rules, and manager.
//!
//! Consolidation promotes frequently-seen episodic entries to semantic
//! memory and compresses groups of episodes into summaries. The
//! ConsolidationManager holds the active rule set and per-namespace
//! metadata such as last run time and cumulative counters.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::llm::client::LlmClient;
use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;

/// Policy for resolving conflicts when a consolidation target already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Skip,
    Overwrite,
    Version,
}

/// Strategy for producing a summary when compressing episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStrategy {
    #[default]
    Concat,
    LlmSummary,
    Dedupe,
}

/// How to group entries for the Compress rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    KeyPrefix,
    Namespace,
}

/// A single consolidation rule.
///
/// - `Promote`: elevate episodic entries that appear frequently enough
///   into a longer-lived memory type (default: Semantic).
/// - `Compress`: merge a group of episodes into a single summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsolidationRule {
    Promote {
        #[serde(default)]
        description: String,
        min_occurrences: usize,
        #[serde(default)]
        lookback_secs: u64,
        key_prefix: String,
        #[serde(default = "default_semantic")]
        target_type: MemoryType,
        #[serde(default)]
        conflict_policy: ConflictPolicy,
    },
    Compress {
        #[serde(default)]
        description: String,
        min_episodes: usize,
        #[serde(default)]
        group_by: GroupBy,
        #[serde(default)]
        summary_strategy: SummaryStrategy,
        target_key: String,
        #[serde(default = "default_semantic")]
        target_type: MemoryType,
        #[serde(default = "default_true")]
        delete_source: bool,
    },
}

fn default_semantic() -> MemoryType {
    MemoryType::Semantic
}

fn default_true() -> bool {
    true
}

/// Record of a single consolidation action for auditing/logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationDetail {
    pub action: String,
    pub from: String,
    pub to: String,
    pub occurrences: usize,
}

/// Aggregate result of a consolidation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationResult {
    pub promoted: usize,
    pub compressed: usize,
    pub skipped: usize,
    pub details: Vec<ConsolidationDetail>,
}

impl ConsolidationResult {
    /// Create a zeroed result.
    pub fn new() -> Self {
        ConsolidationResult {
            promoted: 0,
            compressed: 0,
            skipped: 0,
            details: Vec::new(),
        }
    }
}

impl Default for ConsolidationResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-namespace metadata tracked across consolidation runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConsolidationMeta {
    #[serde(default)]
    pub last_run_ms: u64,
    #[serde(default)]
    pub total_promoted: u64,
    #[serde(default)]
    pub total_compressed: u64,
}

/// Manages consolidation rules and per-namespace metadata.
pub struct ConsolidationManager {
    rules: Mutex<Vec<ConsolidationRule>>,
    meta: Mutex<HashMap<String, ConsolidationMeta>>,
    enabled: bool,
}

impl ConsolidationManager {
    /// Create a new manager with the given rules and enabled flag.
    pub fn new(rules: Vec<ConsolidationRule>, enabled: bool) -> Self {
        ConsolidationManager {
            rules: Mutex::new(rules),
            meta: Mutex::new(HashMap::new()),
            enabled,
        }
    }

    /// Create a disabled manager with no rules.
    pub fn disabled() -> Self {
        Self::new(Vec::new(), false)
    }

    /// Whether consolidation is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get a clone of the current rules.
    pub fn rules(&self) -> Vec<ConsolidationRule> {
        self.rules.lock().unwrap().clone()
    }

    /// Replace the rule set.
    pub fn set_rules(&self, new_rules: Vec<ConsolidationRule>) {
        *self.rules.lock().unwrap() = new_rules;
    }

    /// Get metadata for a namespace (returns a clone).
    pub fn get_meta(&self, namespace: &str) -> Option<ConsolidationMeta> {
        self.meta.lock().unwrap().get(namespace).cloned()
    }

    /// Update (or insert) metadata for a namespace.
    pub fn update_meta(&self, namespace: &str, m: ConsolidationMeta) {
        self.meta.lock().unwrap().insert(namespace.to_string(), m);
    }

    /// List all namespaces that have metadata.
    pub fn namespaces_with_meta(&self) -> Vec<String> {
        self.meta.lock().unwrap().keys().cloned().collect()
    }
}

/// Run consolidation for a single namespace using the provided rules.
/// Returns a ConsolidationResult summarizing what happened.
///
/// This is the synchronous version that falls back to concat_dedupe for
/// LlmSummary strategies. Use `run_consolidation_with_llm` for async LLM support.
pub fn run_consolidation(
    engine: &KvEngine,
    namespace: &str,
    rules: &[ConsolidationRule],
) -> ConsolidationResult {
    let mut result = ConsolidationResult::new();
    for rule in rules {
        match rule {
            ConsolidationRule::Promote {
                min_occurrences,
                key_prefix,
                target_type,
                conflict_policy,
                ..
            } => {
                run_promote_rule(
                    engine,
                    namespace,
                    *min_occurrences,
                    key_prefix,
                    *target_type,
                    *conflict_policy,
                    &mut result,
                );
            }
            ConsolidationRule::Compress {
                min_episodes,
                group_by,
                summary_strategy,
                target_key,
                target_type,
                delete_source,
                ..
            } => {
                run_compress_rule(
                    engine,
                    namespace,
                    *min_episodes,
                    *group_by,
                    *summary_strategy,
                    target_key,
                    *target_type,
                    *delete_source,
                    &mut result,
                );
            }
        }
    }
    result
}

/// Run consolidation with LLM support for the llm_summary strategy.
/// If llm_client is None or disabled, falls back to concat for LLM summaries.
pub async fn run_consolidation_with_llm(
    engine: &KvEngine,
    namespace: &str,
    rules: &[ConsolidationRule],
    llm_client: Option<Arc<LlmClient>>,
) -> ConsolidationResult {
    let mut result = ConsolidationResult::new();
    for rule in rules {
        match rule {
            ConsolidationRule::Promote {
                min_occurrences,
                key_prefix,
                target_type,
                conflict_policy,
                ..
            } => {
                run_promote_rule(
                    engine,
                    namespace,
                    *min_occurrences,
                    key_prefix,
                    *target_type,
                    *conflict_policy,
                    &mut result,
                );
            }
            ConsolidationRule::Compress {
                min_episodes,
                group_by,
                summary_strategy,
                target_key,
                target_type,
                delete_source,
                ..
            } => {
                run_compress_rule_with_llm(
                    engine,
                    namespace,
                    *min_episodes,
                    *group_by,
                    *summary_strategy,
                    target_key,
                    *target_type,
                    *delete_source,
                    &mut result,
                    llm_client.as_ref(),
                )
                .await;
            }
        }
    }
    result
}

/// Handle a single Promote rule: scan episodic entries matching key_prefix,
/// group by suffix, and promote groups that meet the occurrence threshold.
fn run_promote_rule(
    engine: &KvEngine,
    namespace: &str,
    min_occurrences: usize,
    key_prefix: &str,
    target_type: MemoryType,
    conflict_policy: ConflictPolicy,
    result: &mut ConsolidationResult,
) {
    let scan_prefix = format!("{}:{}", namespace, key_prefix);
    let entries = engine.scan_prefix(&scan_prefix);

    // Group by suffix: strip "namespace:" then strip key_prefix,
    // then group by the first segment of the suffix (before any further ':')
    let mut groups: HashMap<String, Vec<(String, Vec<u8>)>> = HashMap::new();
    for (full_key, value, meta) in entries {
        if meta.memory_type != MemoryType::Episodic {
            continue;
        }
        // Strip "namespace:" to get the key portion
        let key_part = match full_key.strip_prefix(&format!("{}:", namespace)) {
            Some(k) => k,
            None => continue,
        };
        // Strip the key_prefix to get the suffix
        let suffix_raw = match key_part.strip_prefix(key_prefix) {
            Some(s) => s,
            None => continue,
        };
        // Group by first segment of the suffix (before any ':')
        let group_key = suffix_raw
            .split(':')
            .next()
            .unwrap_or(suffix_raw)
            .to_string();
        groups.entry(group_key).or_default().push((full_key, value));
    }

    for (suffix, mut entries) in groups {
        if entries.len() < min_occurrences {
            continue;
        }

        // Sort by key so that the "last" entry is the lexicographically latest
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let target_key_suffix = format!("fact:{}", suffix);
        let target_internal = format!("{}:{}", namespace, target_key_suffix);

        // Check for conflict with existing semantic entry
        let existing = engine.get_with_meta(&target_internal);
        if let Some((_, meta)) = &existing
            && meta.memory_type == MemoryType::Semantic
        {
            match conflict_policy {
                ConflictPolicy::Skip => {
                    result.skipped += 1;
                    continue;
                }
                ConflictPolicy::Overwrite => { /* proceed to overwrite below */ }
                ConflictPolicy::Version => {
                    // Find next available version
                    let mut version = 2u32;
                    loop {
                        let versioned_suffix = format!("fact:{}_v{}", suffix, version);
                        let versioned_internal = format!("{}:{}", namespace, versioned_suffix);
                        if engine.get(&versioned_internal).is_none() {
                            // Use this versioned key instead
                            let _ = engine.set(
                                &versioned_internal,
                                entries.last().unwrap().1.clone(),
                                None,
                                target_type,
                            );
                            // Delete all source episodic entries
                            for (src_key, _) in &entries {
                                engine.delete(src_key);
                            }
                            result.details.push(ConsolidationDetail {
                                action: "promote".to_string(),
                                from: format!("{} episodic entries", entries.len()),
                                to: versioned_internal,
                                occurrences: entries.len(),
                            });
                            result.promoted += 1;
                            break;
                        }
                        version += 1;
                    }
                    continue;
                }
            }
        }

        // Write the latest value from the group as the target type (no TTL)
        let latest_value = entries.last().unwrap().1.clone();
        let _ = engine.set(&target_internal, latest_value, None, target_type);

        // Delete all source episodic entries
        for (src_key, _) in &entries {
            engine.delete(src_key);
        }

        result.details.push(ConsolidationDetail {
            action: "promote".to_string(),
            from: format!("{} episodic entries", entries.len()),
            to: target_internal,
            occurrences: entries.len(),
        });
        result.promoted += 1;
    }
}

/// Handle a single Compress rule: scan episodic entries, group them,
/// and produce summary entries for groups that meet the episode threshold.
#[allow(clippy::too_many_arguments)]
fn run_compress_rule(
    engine: &KvEngine,
    namespace: &str,
    min_episodes: usize,
    group_by: GroupBy,
    summary_strategy: SummaryStrategy,
    target_key: &str,
    target_type: MemoryType,
    delete_source: bool,
    result: &mut ConsolidationResult,
) {
    let groups = group_episodic_entries(engine, namespace, group_by);

    for (group_key, entries) in groups {
        if entries.len() < min_episodes {
            continue;
        }

        // Build summary
        let summary = match summary_strategy {
            SummaryStrategy::Concat => concat_dedupe(&entries),
            SummaryStrategy::Dedupe => {
                let unique: HashSet<Vec<u8>> = entries.iter().map(|(_, v)| v.clone()).collect();
                let parts: Vec<String> = unique
                    .iter()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
                    .collect();
                parts.join("; ")
            }
            SummaryStrategy::LlmSummary => {
                // LLM summary not available in sync path; fall back to concat
                concat_dedupe(&entries)
            }
        };

        let final_key = target_key.replace("{prefix}", &group_key);
        let target_internal = format!("{}:{}", namespace, final_key);

        let _ = engine.set(&target_internal, summary.into_bytes(), None, target_type);

        if delete_source {
            for (src_key, _) in &entries {
                engine.delete(src_key);
            }
        }

        result.details.push(ConsolidationDetail {
            action: "compress".to_string(),
            from: format!("{} episodes in group '{}'", entries.len(), group_key),
            to: target_internal,
            occurrences: entries.len(),
        });
        result.compressed += 1;
    }
}

/// Async variant of run_compress_rule with LLM summarization support.
/// For LlmSummary strategy, calls the LLM client if available and enabled;
/// otherwise falls back to concat_dedupe.
#[allow(clippy::too_many_arguments)]
async fn run_compress_rule_with_llm(
    engine: &KvEngine,
    namespace: &str,
    min_episodes: usize,
    group_by: GroupBy,
    summary_strategy: SummaryStrategy,
    target_key: &str,
    target_type: MemoryType,
    delete_source: bool,
    result: &mut ConsolidationResult,
    llm_client: Option<&Arc<LlmClient>>,
) {
    let groups = group_episodic_entries(engine, namespace, group_by);

    for (group_key, entries) in groups {
        if entries.len() < min_episodes {
            continue;
        }

        // Build summary
        let summary = match summary_strategy {
            SummaryStrategy::Concat => concat_dedupe(&entries),
            SummaryStrategy::Dedupe => {
                let unique: HashSet<Vec<u8>> = entries.iter().map(|(_, v)| v.clone()).collect();
                let parts: Vec<String> = unique
                    .iter()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
                    .collect();
                parts.join("; ")
            }
            SummaryStrategy::LlmSummary => {
                let values: Vec<String> = entries
                    .iter()
                    .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
                    .collect();
                let context = format!("Consolidating episodic memories in group '{}'", group_key);
                match llm_client {
                    Some(client) if client.is_enabled() => {
                        match client.summarize(&values, &context).await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(
                                    "LLM summarization failed for group '{}': {}; falling back to concat",
                                    group_key,
                                    e
                                );
                                concat_dedupe(&entries)
                            }
                        }
                    }
                    _ => {
                        // No LLM client or client is disabled; fall back to concat
                        concat_dedupe(&entries)
                    }
                }
            }
        };

        let final_key = target_key.replace("{prefix}", &group_key);
        let target_internal = format!("{}:{}", namespace, final_key);

        let _ = engine.set(&target_internal, summary.into_bytes(), None, target_type);

        if delete_source {
            for (src_key, _) in &entries {
                engine.delete(src_key);
            }
        }

        result.details.push(ConsolidationDetail {
            action: "compress".to_string(),
            from: format!("{} episodes in group '{}'", entries.len(), group_key),
            to: target_internal,
            occurrences: entries.len(),
        });
        result.compressed += 1;
    }
}

/// Scan episodic entries in a namespace and group them by the chosen strategy.
fn group_episodic_entries(
    engine: &KvEngine,
    namespace: &str,
    group_by: GroupBy,
) -> HashMap<String, Vec<(String, Vec<u8>)>> {
    let scan_prefix = format!("{}:", namespace);
    let entries = engine.scan_prefix(&scan_prefix);

    // Only consider episodic entries
    let episodic: Vec<(String, Vec<u8>)> = entries
        .into_iter()
        .filter(|(_, _, meta)| meta.memory_type == MemoryType::Episodic)
        .map(|(k, v, _)| (k, v))
        .collect();

    // Group by the chosen strategy
    let mut groups: HashMap<String, Vec<(String, Vec<u8>)>> = HashMap::new();
    for (full_key, value) in episodic {
        let group_key = match group_by {
            GroupBy::KeyPrefix => {
                // First segment before ':' after stripping namespace
                let key_part = match full_key.strip_prefix(&format!("{}:", namespace)) {
                    Some(k) => k,
                    None => continue,
                };
                match key_part.split(':').next() {
                    Some(prefix) => prefix.to_string(),
                    None => key_part.to_string(),
                }
            }
            GroupBy::Namespace => "__all__".to_string(),
        };
        groups.entry(group_key).or_default().push((full_key, value));
    }
    groups
}

/// Deduplicate values and join with newline separator.
fn concat_dedupe(entries: &[(String, Vec<u8>)]) -> String {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    for (_, value) in entries {
        if seen.insert(value.clone()) {
            parts.push(String::from_utf8_lossy(value).into_owned());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_policy_default() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Skip);
    }

    #[test]
    fn test_summary_strategy_default() {
        assert_eq!(SummaryStrategy::default(), SummaryStrategy::Concat);
    }

    #[test]
    fn test_group_by_default() {
        assert_eq!(GroupBy::default(), GroupBy::KeyPrefix);
    }

    #[test]
    fn test_rule_promote_serde_roundtrip() {
        let json = serde_json::json!({
            "type": "promote",
            "description": "promote frequent events",
            "min_occurrences": 3,
            "lookback_secs": 3600,
            "key_prefix": "event:",
            "target_type": "semantic",
            "conflict_policy": "overwrite"
        });
        let rule: ConsolidationRule = serde_json::from_value(json.clone()).unwrap();
        if let ConsolidationRule::Promote {
            description,
            min_occurrences,
            lookback_secs,
            key_prefix,
            target_type,
            conflict_policy,
        } = &rule
        {
            assert_eq!(description, "promote frequent events");
            assert_eq!(*min_occurrences, 3);
            assert_eq!(*lookback_secs, 3600);
            assert_eq!(key_prefix, "event:");
            assert_eq!(*target_type, MemoryType::Semantic);
            assert_eq!(*conflict_policy, ConflictPolicy::Overwrite);
        } else {
            panic!("expected Promote variant");
        }
        // Round-trip back to JSON
        let back = serde_json::to_value(&rule).unwrap();
        assert_eq!(back["type"], "promote");
        assert_eq!(back["min_occurrences"], 3);
    }

    #[test]
    fn test_rule_compress_serde_roundtrip() {
        let json = serde_json::json!({
            "type": "compress",
            "description": "summarize episodes",
            "min_episodes": 5,
            "group_by": "namespace",
            "summary_strategy": "llm_summary",
            "target_key": "summary:{prefix}",
            "target_type": "semantic",
            "delete_source": false
        });
        let rule: ConsolidationRule = serde_json::from_value(json.clone()).unwrap();
        if let ConsolidationRule::Compress {
            description,
            min_episodes,
            group_by,
            summary_strategy,
            target_key,
            target_type,
            delete_source,
        } = &rule
        {
            assert_eq!(description, "summarize episodes");
            assert_eq!(*min_episodes, 5);
            assert_eq!(*group_by, GroupBy::Namespace);
            assert_eq!(*summary_strategy, SummaryStrategy::LlmSummary);
            assert_eq!(target_key, "summary:{prefix}");
            assert_eq!(*target_type, MemoryType::Semantic);
            assert!(!delete_source);
        } else {
            panic!("expected Compress variant");
        }
        let back = serde_json::to_value(&rule).unwrap();
        assert_eq!(back["type"], "compress");
        assert_eq!(back["min_episodes"], 5);
    }

    #[test]
    fn test_rule_defaults_serde() {
        // Promote with only required fields -- defaults should fill in
        let json = serde_json::json!({
            "type": "promote",
            "min_occurrences": 2,
            "key_prefix": "log:"
        });
        let rule: ConsolidationRule = serde_json::from_value(json).unwrap();
        if let ConsolidationRule::Promote {
            description,
            min_occurrences,
            lookback_secs,
            key_prefix,
            target_type,
            conflict_policy,
        } = &rule
        {
            assert_eq!(description, "");
            assert_eq!(*min_occurrences, 2);
            assert_eq!(*lookback_secs, 0);
            assert_eq!(key_prefix, "log:");
            assert_eq!(*target_type, MemoryType::Semantic);
            assert_eq!(*conflict_policy, ConflictPolicy::Skip);
        } else {
            panic!("expected Promote variant");
        }

        // Compress with only required fields
        let json = serde_json::json!({
            "type": "compress",
            "min_episodes": 4,
            "target_key": "summary:{prefix}"
        });
        let rule: ConsolidationRule = serde_json::from_value(json).unwrap();
        if let ConsolidationRule::Compress {
            description,
            min_episodes,
            group_by,
            summary_strategy,
            target_key,
            target_type,
            delete_source,
        } = &rule
        {
            assert_eq!(description, "");
            assert_eq!(*min_episodes, 4);
            assert_eq!(*group_by, GroupBy::KeyPrefix);
            assert_eq!(*summary_strategy, SummaryStrategy::Concat);
            assert_eq!(target_key, "summary:{prefix}");
            assert_eq!(*target_type, MemoryType::Semantic);
            assert!(*delete_source);
        } else {
            panic!("expected Compress variant");
        }
    }

    #[test]
    fn test_manager_disabled() {
        let mgr = ConsolidationManager::disabled();
        assert!(!mgr.is_enabled());
        assert!(mgr.rules().is_empty());
    }

    #[test]
    fn test_manager_with_rules() {
        let rules = vec![
            ConsolidationRule::Promote {
                description: "promote events".to_string(),
                min_occurrences: 3,
                lookback_secs: 3600,
                key_prefix: "event:".to_string(),
                target_type: MemoryType::Semantic,
                conflict_policy: ConflictPolicy::Skip,
            },
            ConsolidationRule::Compress {
                description: "compress logs".to_string(),
                min_episodes: 5,
                group_by: GroupBy::KeyPrefix,
                summary_strategy: SummaryStrategy::Concat,
                target_key: "summary:{prefix}".to_string(),
                target_type: MemoryType::Semantic,
                delete_source: true,
            },
        ];
        let mgr = ConsolidationManager::new(rules.clone(), true);
        assert!(mgr.is_enabled());
        assert_eq!(mgr.rules().len(), 2);

        // set_rules replaces
        mgr.set_rules(vec![]);
        assert!(mgr.rules().is_empty());
    }

    #[test]
    fn test_consolidation_meta_default() {
        let meta = ConsolidationMeta::default();
        assert_eq!(meta.last_run_ms, 0);
        assert_eq!(meta.total_promoted, 0);
        assert_eq!(meta.total_compressed, 0);
    }

    #[test]
    fn test_consolidation_result_new() {
        let result = ConsolidationResult::new();
        assert_eq!(result.promoted, 0);
        assert_eq!(result.compressed, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.details.is_empty());
    }

    #[test]
    fn test_manager_meta_operations() {
        let mgr = ConsolidationManager::disabled();

        // No meta initially
        assert!(mgr.get_meta("ns1").is_none());
        assert!(mgr.namespaces_with_meta().is_empty());

        // Insert meta
        let meta = ConsolidationMeta {
            last_run_ms: 1000,
            total_promoted: 5,
            total_compressed: 2,
        };
        mgr.update_meta("ns1", meta);

        let retrieved = mgr.get_meta("ns1").unwrap();
        assert_eq!(retrieved.last_run_ms, 1000);
        assert_eq!(retrieved.total_promoted, 5);
        assert_eq!(retrieved.total_compressed, 2);

        let ns = mgr.namespaces_with_meta();
        assert_eq!(ns.len(), 1);
        assert!(ns.contains(&"ns1".to_string()));
    }

    // --- Runner logic tests ---

    use crate::store::engine::KvEngine;
    use std::sync::Arc;

    fn make_engine() -> Arc<KvEngine> {
        Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())))
    }

    #[test]
    fn test_promote_basic() {
        let engine = make_engine();
        let namespace = "test";
        // Insert 3 episodic entries with keys sharing the same suffix group "ping"
        for i in 0..3 {
            let key = format!("{}:event:ping:{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("val{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Promote {
            description: "promote pings".to_string(),
            min_occurrences: 3,
            lookback_secs: 0,
            key_prefix: "event:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Skip,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.promoted, 1);
        assert_eq!(result.skipped, 0);

        // The semantic entry should exist at fact:ping
        let target = format!("{}:fact:ping", namespace);
        let val = engine.get(&target).unwrap();
        assert_eq!(val, b"val2"); // latest value (from ping:2)

        // The episodic entries should be gone
        for i in 0..3 {
            let src = format!("{}:event:ping:{}", namespace, i);
            assert!(
                engine.get(&src).is_none(),
                "source entry {} should be deleted",
                i
            );
        }
    }

    #[test]
    fn test_promote_below_threshold() {
        let engine = make_engine();
        let namespace = "test";
        // Insert only 2 episodic entries, but min_occurrences=3
        for i in 0..2 {
            let key = format!("{}:event:ping:{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("val{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Promote {
            description: "promote pings".to_string(),
            min_occurrences: 3,
            lookback_secs: 0,
            key_prefix: "event:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Skip,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped, 0);

        // Episodic entries should still exist
        for i in 0..2 {
            let src = format!("{}:event:ping:{}", namespace, i);
            assert!(engine.get(&src).is_some());
        }
    }

    #[test]
    fn test_promote_conflict_skip() {
        let engine = make_engine();
        let namespace = "test";
        // Pre-existing semantic entry at the target key
        let target = format!("{}:fact:ping", namespace);
        engine
            .set(&target, b"existing".to_vec(), None, MemoryType::Semantic)
            .unwrap();

        // Insert 3 episodic entries
        for i in 0..3 {
            let key = format!("{}:event:ping:{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("val{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Promote {
            description: "promote pings".to_string(),
            min_occurrences: 3,
            lookback_secs: 0,
            key_prefix: "event:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Skip,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped, 1);

        // Original semantic value should remain
        let val = engine.get(&target).unwrap();
        assert_eq!(val, b"existing");
    }

    #[test]
    fn test_promote_conflict_overwrite() {
        let engine = make_engine();
        let namespace = "test";
        // Pre-existing semantic entry at the target key
        let target = format!("{}:fact:ping", namespace);
        engine
            .set(&target, b"old".to_vec(), None, MemoryType::Semantic)
            .unwrap();

        // Insert 3 episodic entries
        for i in 0..3 {
            let key = format!("{}:event:ping:{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("val{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Promote {
            description: "promote pings".to_string(),
            min_occurrences: 3,
            lookback_secs: 0,
            key_prefix: "event:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Overwrite,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.promoted, 1);
        assert_eq!(result.skipped, 0);

        // Should have been overwritten with latest value
        let val = engine.get(&target).unwrap();
        assert_eq!(val, b"val2");
    }

    #[test]
    fn test_promote_conflict_version() {
        let engine = make_engine();
        let namespace = "test";
        // Pre-existing semantic entry at the target key
        let target = format!("{}:fact:ping", namespace);
        engine
            .set(&target, b"v1".to_vec(), None, MemoryType::Semantic)
            .unwrap();

        // Insert 3 episodic entries
        for i in 0..3 {
            let key = format!("{}:event:ping:{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("val{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Promote {
            description: "promote pings".to_string(),
            min_occurrences: 3,
            lookback_secs: 0,
            key_prefix: "event:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Version,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.promoted, 1);
        assert_eq!(result.skipped, 0);

        // v2 should have been created
        let v2_key = format!("{}:fact:ping_v2", namespace);
        let val = engine.get(&v2_key).unwrap();
        assert_eq!(val, b"val2");

        // Original should remain
        let val = engine.get(&target).unwrap();
        assert_eq!(val, b"v1");

        // Episodic entries should be gone
        for i in 0..3 {
            let src = format!("{}:event:ping:{}", namespace, i);
            assert!(
                engine.get(&src).is_none(),
                "source entry {} should be deleted",
                i
            );
        }
    }

    #[test]
    fn test_compress_concat() {
        let engine = make_engine();
        let namespace = "test";
        // Insert 5 episodic entries with the same key prefix
        for i in 0..5 {
            let key = format!("{}:log:entry{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("data{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Compress {
            description: "compress logs".to_string(),
            min_episodes: 5,
            group_by: GroupBy::KeyPrefix,
            summary_strategy: SummaryStrategy::Concat,
            target_key: "summary:{prefix}".to_string(),
            target_type: MemoryType::Semantic,
            delete_source: false,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.compressed, 1);

        // Summary should exist
        let target = format!("{}:summary:log", namespace);
        let val = engine.get(&target).unwrap();
        let summary = String::from_utf8(val).unwrap();
        // Should contain all 5 data values, newline-separated
        for i in 0..5 {
            assert!(
                summary.contains(&format!("data{}", i)),
                "summary missing data{}",
                i
            );
        }
    }

    #[test]
    fn test_compress_below_threshold() {
        let engine = make_engine();
        let namespace = "test";
        // Insert only 3 episodic entries, but min_episodes=5
        for i in 0..3 {
            let key = format!("{}:log:entry{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("data{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Compress {
            description: "compress logs".to_string(),
            min_episodes: 5,
            group_by: GroupBy::KeyPrefix,
            summary_strategy: SummaryStrategy::Concat,
            target_key: "summary:{prefix}".to_string(),
            target_type: MemoryType::Semantic,
            delete_source: false,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.compressed, 0);

        // No summary should exist
        let target = format!("{}:summary:log", namespace);
        assert!(engine.get(&target).is_none());
    }

    #[test]
    fn test_compress_delete_source() {
        let engine = make_engine();
        let namespace = "test";
        // Insert 5 episodic entries
        for i in 0..5 {
            let key = format!("{}:log:entry{}", namespace, i);
            engine
                .set(
                    &key,
                    format!("data{}", i).into_bytes(),
                    None,
                    MemoryType::Episodic,
                )
                .unwrap();
        }

        let rules = vec![ConsolidationRule::Compress {
            description: "compress logs".to_string(),
            min_episodes: 5,
            group_by: GroupBy::KeyPrefix,
            summary_strategy: SummaryStrategy::Concat,
            target_key: "summary:{prefix}".to_string(),
            target_type: MemoryType::Semantic,
            delete_source: true,
        }];

        let result = run_consolidation(&engine, namespace, &rules);
        assert_eq!(result.compressed, 1);

        // Summary should exist
        let target = format!("{}:summary:log", namespace);
        assert!(engine.get(&target).is_some());

        // Source entries should be deleted
        for i in 0..5 {
            let key = format!("{}:log:entry{}", namespace, i);
            assert!(
                engine.get(&key).is_none(),
                "source entry {} should be deleted",
                i
            );
        }
    }
}
