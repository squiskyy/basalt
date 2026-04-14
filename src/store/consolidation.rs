//! Memory consolidation: types, rules, and manager.
//!
//! Consolidation promotes frequently-seen episodic entries to semantic
//! memory and compresses groups of episodes into summaries. The
//! ConsolidationManager holds the active rule set and per-namespace
//! metadata such as last run time and cumulative counters.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::store::memory_type::MemoryType;

/// Policy for resolving conflicts when a consolidation target already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Version,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        ConflictPolicy::Skip
    }
}

/// Strategy for producing a summary when compressing episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStrategy {
    Concat,
    LlmSummary,
    Dedupe,
}

impl Default for SummaryStrategy {
    fn default() -> Self {
        SummaryStrategy::Concat
    }
}

/// How to group entries for the Compress rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    KeyPrefix,
    Namespace,
}

impl Default for GroupBy {
    fn default() -> Self {
        GroupBy::KeyPrefix
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationMeta {
    #[serde(default)]
    pub last_run_ms: u64,
    #[serde(default)]
    pub total_promoted: u64,
    #[serde(default)]
    pub total_compressed: u64,
}

impl Default for ConsolidationMeta {
    fn default() -> Self {
        ConsolidationMeta {
            last_run_ms: 0,
            total_promoted: 0,
            total_compressed: 0,
        }
    }
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
}
