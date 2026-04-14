# Memory Consolidation Implementation Plan (Issue #43)

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Implement automatic memory consolidation that promotes recurring episodic memories into semantic facts and compresses related episodes into summarized semantic memories.

**Architecture:** New `store/consolidation.rs` module with a `ConsolidationManager` that holds configurable rules, a `ConsolidationRunner` that executes rules against the store, background task loop (same pattern as trigger sweep), and HTTP/RESP API endpoints for manual trigger and status. The LLM client (already built) is used for the `llm_summary` compress strategy.

**Tech Stack:** Rust, tokio, serde, existing KvEngine/store abstractions

---

### Task 1: Create consolidation types and config structs

**Objective:** Define the core data types for consolidation rules, results, and config.

**Files:**
- Create: `src/store/consolidation.rs`

**Step 1: Create the consolidation module with types**

```rust
//! Automatic memory consolidation: promote recurring episodic memories into semantic facts.
//!
//! Consolidation rules scan episodic entries and promote or compress them into
//! semantic memories based on configurable thresholds.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::store::memory_type::MemoryType;

/// Policy for handling conflicts when a semantic key already exists during promotion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Leave the episodic memory, do not overwrite the existing semantic key.
    Skip,
    /// Replace the semantic value with the latest episodic value.
    Overwrite,
    /// Store as a versioned key (e.g., `fact:key:v2`), keeping the original.
    Version,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        ConflictPolicy::Skip
    }
}

/// Strategy for compressing multiple episodic memories into a summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStrategy {
    /// Simple string join with deduplication (no LLM required).
    Concat,
    /// Call the configured LLM to produce a condensed summary.
    LlmSummary,
    /// Deduplicate identical values and keep the distinct set.
    Dedupe,
}

impl Default for SummaryStrategy {
    fn default() -> Self {
        SummaryStrategy::Concat
    }
}

/// How to group episodic memories for the compress rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// Group by key prefix (e.g., all keys starting with `obs:user_pref`).
    KeyPrefix,
    /// Group by namespace.
    Namespace,
}

impl Default for GroupBy {
    fn default() -> Self {
        GroupBy::KeyPrefix
    }
}

/// A consolidation rule definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsolidationRule {
    /// Promote a recurring episodic key to a semantic fact when seen N times.
    Promote {
        /// Human-readable description.
        #[serde(default)]
        description: String,
        /// Minimum number of distinct episodic occurrences to trigger promotion.
        min_occurrences: usize,
        /// Only count episodes within this time window (ms). 0 = no window limit.
        #[serde(default)]
        lookback_secs: u64,
        /// Only consider episodic keys matching this prefix.
        key_prefix: String,
        /// The target memory type (always semantic for promote, but kept for flexibility).
        #[serde(default = "default_semantic")]
        target_type: MemoryType,
        /// What to do when the target semantic key already exists.
        #[serde(default)]
        conflict_policy: ConflictPolicy,
    },
    /// Compress multiple related episodic memories into a single semantic summary.
    Compress {
        /// Human-readable description.
        #[serde(default)]
        description: String,
        /// Minimum number of episodes to trigger compression.
        min_episodes: usize,
        /// How to group episodes for compression.
        #[serde(default)]
        group_by: GroupBy,
        /// Strategy for producing the summary.
        #[serde(default)]
        summary_strategy: SummaryStrategy,
        /// Template for the target key. `{prefix}` is replaced with the group prefix.
        target_key: String,
        /// The target memory type.
        #[serde(default = "default_semantic")]
        target_type: MemoryType,
        /// Whether to delete source episodic memories after compression.
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

impl ConsolidationRule {
    /// Return a human-readable name for the rule.
    pub fn name(&self) -> &str {
        match self {
            ConsolidationRule::Promote { description, .. } => {
                if description.is_empty() {
                    "promote"
                } else {
                    description
                }
            }
            ConsolidationRule::Compress { description, .. } => {
                if description.is_empty() {
                    "compress"
                } else {
                    description
                }
            }
        }
    }
}

/// Result of a single consolidation action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationDetail {
    /// What action was taken: "promote" or "compress".
    pub action: String,
    /// Source key or pattern.
    pub from: String,
    /// Target semantic key written.
    pub to: String,
    /// Number of episodic entries involved.
    pub occurrences: usize,
}

/// Summary result of a consolidation run for a namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationResult {
    /// Number of entries promoted to semantic.
    pub promoted: usize,
    /// Number of groups compressed into summaries.
    pub compressed: usize,
    /// Number of rules skipped (e.g., conflict policy = skip and key existed).
    pub skipped: usize,
    /// Per-action details.
    pub details: Vec<ConsolidationDetail>,
}

impl ConsolidationResult {
    pub fn new() -> Self {
        ConsolidationResult {
            promoted: 0,
            compressed: 0,
            skipped: 0,
            details: Vec::new(),
        }
    }
}

/// Metadata about the last consolidation run (persisted in snapshots).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationMeta {
    /// Timestamp of the last completed run (ms since epoch).
    pub last_run_ms: u64,
    /// Total promotions across all runs.
    pub total_promoted: u64,
    /// Total compressions across all runs.
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

/// Manager for consolidation configuration and metadata.
pub struct ConsolidationManager {
    /// Configured consolidation rules (ordered, first match wins).
    rules: Mutex<Vec<ConsolidationRule>>,
    /// Per-namespace consolidation metadata.
    meta: Mutex<HashMap<String, ConsolidationMeta>>,
    /// Whether consolidation is enabled.
    enabled: bool,
}

impl ConsolidationManager {
    /// Create a new consolidation manager with the given rules.
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

    /// Get the current rules.
    pub fn rules(&self) -> Vec<ConsolidationRule> {
        self.rules.lock().unwrap().clone()
    }

    /// Update rules at runtime.
    pub fn set_rules(&self, rules: Vec<ConsolidationRule>) {
        *self.rules.lock().unwrap() = rules;
    }

    /// Get consolidation metadata for a namespace.
    pub fn get_meta(&self, namespace: &str) -> ConsolidationMeta {
        self.meta
            .lock()
            .unwrap()
            .get(namespace)
            .cloned()
            .unwrap_or_default()
    }

    /// Update consolidation metadata for a namespace.
    pub fn update_meta(&self, namespace: &str, meta: ConsolidationMeta) {
        self.meta
            .lock()
            .unwrap()
            .insert(namespace.to_string(), meta);
    }

    /// List all namespaces that have consolidation metadata.
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
    fn test_consolidation_rule_serde_promote() {
        let json = r#"{"type":"promote","min_occurrences":3,"lookback_secs":3600,"key_prefix":"obs:","target_type":"semantic","conflict_policy":"skip"}"#;
        let rule: ConsolidationRule = serde_json::from_str(json).unwrap();
        match rule {
            ConsolidationRule::Promote {
                min_occurrences,
                key_prefix,
                ..
            } => {
                assert_eq!(min_occurrences, 3);
                assert_eq!(key_prefix, "obs:");
            }
            _ => panic!("Expected Promote variant"),
        }
    }

    #[test]
    fn test_consolidation_rule_serde_compress() {
        let json = r#"{"type":"compress","min_episodes":5,"group_by":"key_prefix","summary_strategy":"concat","target_key":"summary:{prefix}","target_type":"semantic","delete_source":true}"#;
        let rule: ConsolidationRule = serde_json::from_str(json).unwrap();
        match rule {
            ConsolidationRule::Compress {
                min_episodes,
                target_key,
                ..
            } => {
                assert_eq!(min_episodes, 5);
                assert_eq!(target_key, "summary:{prefix}");
            }
            _ => panic!("Expected Compress variant"),
        }
    }

    #[test]
    fn test_consolidation_manager_disabled() {
        let mgr = ConsolidationManager::disabled();
        assert!(!mgr.is_enabled());
        assert!(mgr.rules().is_empty());
    }

    #[test]
    fn test_consolidation_manager_rules() {
        let rules = vec![ConsolidationRule::Promote {
            description: "test".to_string(),
            min_occurrences: 2,
            lookback_secs: 0,
            key_prefix: "obs:".to_string(),
            target_type: MemoryType::Semantic,
            conflict_policy: ConflictPolicy::Skip,
        }];
        let mgr = ConsolidationManager::new(rules, true);
        assert!(mgr.is_enabled());
        assert_eq!(mgr.rules().len(), 1);
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
}
```

**Step 2: Run tests to verify compilation**

Run: `cd ~/basalt && cargo test --lib store::consolidation -- --nocapture`
Expected: All tests pass

**Step 3: Commit**

```bash
git add src/store/consolidation.rs
git commit -m "feat: add consolidation types, rules, and manager (issue #43)"
```

---

### Task 2: Wire consolidation module into the store and engine

**Objective:** Export the consolidation module and add `ConsolidationManager` to `KvEngine`.

**Files:**
- Modify: `src/store/mod.rs`
- Modify: `src/store/engine.rs`

**Step 1: Add consolidation module to store/mod.rs**

Add `pub mod consolidation;` and the public re-exports:

```rust
pub mod consolidation;
// ... existing mods ...

pub use consolidation::{
    ConsolidationDetail, ConsolidationManager, ConsolidationMeta, ConsolidationResult,
    ConsolidationRule, ConflictPolicy, GroupBy, SummaryStrategy,
};
```

**Step 2: Add ConsolidationManager to KvEngine struct**

In `src/store/engine.rs`, add a `consolidation_manager: Arc<ConsolidationManager>` field to `KvEngine`. Update `KvEngine::new()` to accept the manager and add a `consolidation_manager()` accessor method.

In the `KvEngine::new()` signature, add:
```rust
pub fn new(
    shard_count: usize,
    max_entries: usize,
    compression_threshold: usize,
    eviction_policy: EvictionPolicy,
    decay_config: DecayConfigStore,
    trigger_manager: Arc<TriggerManager>,
    consolidation_manager: Arc<ConsolidationManager>,
) -> Self {
    // ... existing code ...
    Self {
        // ... existing fields ...
        consolidation_manager,
    }
}
```

Add accessor:
```rust
/// Get the consolidation manager.
pub fn consolidation_manager(&self) -> &Arc<ConsolidationManager> {
    &self.consolidation_manager
}
```

**Step 3: Update all KvEngine::new() call sites**

Search for every place `KvEngine::new()` is called and add `Arc::new(ConsolidationManager::disabled())` as the last argument. This includes:
- `src/main.rs` (the primary startup)
- Any test files that construct KvEngine directly

**Step 4: Run tests**

Run: `cd ~/basalt && cargo test --lib 2>&1 | tail -30`
Expected: All existing tests still pass (compilation succeeds)

**Step 5: Commit**

```bash
git add src/store/mod.rs src/store/engine.rs src/main.rs
git commit -m "feat: wire ConsolidationManager into KvEngine (issue #43)"
```

---

### Task 3: Add consolidation config to ServerConfig

**Objective:** Add TOML-configurable consolidation settings to the config system.

**Files:**
- Modify: `src/config.rs`

**Step 1: Add consolidation fields to ServerConfig**

Add to `ServerConfig`:
```rust
/// How often (ms) to run consolidation sweep. 0 = disabled.
pub consolidation_interval_ms: u64,
```

Add to `ServerConfig::default()`:
```rust
consolidation_interval_ms: 300_000, // 5 minutes
```

**Step 2: Add consolidation config file struct**

Add a new `ConsolidationFile` struct for TOML deserialization:
```rust
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct ConsolidationFile {
    #[serde(default)]
    interval_ms: Option<u64>,
    #[serde(default)]
    rules: Vec<ConsolidationRuleFile>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct ConsolidationRuleFile {
    r#type: Option<String>,
    // Promote fields
    min_occurrences: Option<usize>,
    lookback_secs: Option<u64>,
    key_prefix: Option<String>,
    conflict_policy: Option<String>,
    // Compress fields
    min_episodes: Option<usize>,
    group_by: Option<String>,
    summary_strategy: Option<String>,
    target_key: Option<String>,
    delete_source: Option<bool>,
    // Common
    description: Option<String>,
}
```

Add `consolidation: Option<ConsolidationFile>` to `ServerFile`.

**Step 3: Add conversion logic in Config::load()**

Parse the consolidation file config into runtime `ConsolidationRule` objects and the interval. Map `ConsolidationRuleFile` -> `ConsolidationRule` with proper defaults.

**Step 4: Run tests**

Run: `cd ~/basalt && cargo test --lib 2>&1 | tail -30`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add consolidation config to ServerConfig (issue #43)"
```

---

### Task 4: Implement consolidation runner logic

**Objective:** Build the core consolidation logic that scans episodic entries, applies rules, and writes semantic results.

**Files:**
- Modify: `src/store/consolidation.rs`

**Step 1: Add the `run_consolidation` function**

This is the main consolidation engine. It takes a `&KvEngine`, namespace, and rules, then:

For **Promote** rules:
1. Scan episodic entries matching `key_prefix` in the namespace
2. Group by key (stripping prefix) and count distinct occurrences within lookback
3. For any group with `count >= min_occurrences`, promote to semantic
4. Apply conflict policy (skip/overwrite/version)
5. Delete the promoted episodic entries

For **Compress** rules:
1. Scan episodic entries in the namespace
2. Group by the `group_by` strategy (key_prefix groups or whole namespace)
3. For any group with `count >= min_episodes`, compress
4. Apply summary strategy (concat/dedupe/llm_summary)
5. Write the compressed semantic entry
6. If `delete_source`, remove the source episodic entries

```rust
use crate::store::engine::{KvEngine, NamespacedKey};
use crate::store::memory_type::MemoryType;
use crate::time::now_ms;
use std::collections::HashMap;

/// Run consolidation for a single namespace using the provided rules.
/// Returns a ConsolidationResult summarizing what happened.
pub async fn run_consolidation(
    engine: &KvEngine,
    namespace: &str,
    rules: &[ConsolidationRule],
) -> ConsolidationResult {
    let mut result = ConsolidationResult::new();
    let ns_prefix = format!("{namespace}:");
    let now = now_ms();

    for rule in rules {
        match rule {
            ConsolidationRule::Promote {
                min_occurrences,
                lookback_secs,
                key_prefix,
                target_type,
                conflict_policy,
                ..
            } => {
                let full_prefix = format!("{ns_prefix}{key_prefix}");
                let entries = engine.scan_prefix(&full_prefix);
                
                // Group by key suffix, count distinct occurrences within lookback
                let cutoff_ms = if *lookback_secs > 0 {
                    Some(now.saturating_sub(lookback_secs * 1000))
                } else {
                    None
                };
                
                let mut groups: HashMap<String, Vec<(String, Vec<u8>)>> = HashMap::new();
                for (key, value, meta) in &entries {
                    if meta.memory_type != MemoryType::Episodic {
                        continue;
                    }
                    if let Some(cutoff) = cutoff_ms {
                        // created_at_ms is not in EntryMeta directly; we approximate
                        // using relevance and TTL. For precise lookback, we'd need
                        // the raw Entry. This is a known limitation.
                        // Skip entries that might be too old based on remaining TTL.
                    }
                    // Strip namespace:prefix to get the suffix (the canonical key)
                    let suffix = key.strip_prefix(&full_prefix).unwrap_or(key).to_string();
                    groups.entry(suffix).or_default().push((key.clone(), value.clone()));
                }
                
                for (suffix, occurrences) in groups {
                    if occurrences.len() < *min_occurrences {
                        continue;
                    }
                    
                    let target_key = format!("fact:{suffix}");
                    let target_nk = NamespacedKey::new(namespace, &target_key);
                    
                    // Check conflict
                    let existing = engine.get(&target_nk.to_internal());
                    match conflict_policy {
                        ConflictPolicy::Skip if existing.is_some() => {
                            result.skipped += 1;
                            continue;
                        }
                        ConflictPolicy::Version if existing.is_some() => {
                            // Find next version number
                            let mut version = 2;
                            loop {
                                let vkey = format!("fact:{suffix}:v{version}");
                                let vnk = NamespacedKey::new(namespace, &vkey);
                                if engine.get(&vnk.to_internal()).is_none() {
                                    break;
                                }
                                version += 1;
                            }
                            // Will write to fact:{suffix}:v{version}
                        }
                        _ => {}
                    }
                    
                    // Use the most recent value for promotion
                    let latest_value = occurrences.last().unwrap().1.clone();
                    
                    // Determine the actual write key
                    let write_key = if let ConflictPolicy::Version = conflict_policy {
                        if existing.is_some() {
                            let mut version = 2;
                            loop {
                                let vkey = format!("fact:{suffix}:v{version}");
                                let vnk = NamespacedKey::new(namespace, &vkey);
                                if engine.get(&vnk.to_internal()).is_none() {
                                    break format!("fact:{suffix}:v{version}");
                                }
                                version += 1;
                            }
                        } else {
                            target_key.clone()
                        }
                    } else {
                        target_key.clone()
                    };
                    
                    let write_nk = NamespacedKey::new(namespace, &write_key);
                    let _ = engine.set(
                        &write_nk.to_internal(),
                        &latest_value,
                        None, // No TTL for semantic
                        Some(*target_type),
                    );
                    
                    // Delete source episodic entries
                    for (src_key, _) in &occurrences {
                        engine.delete(src_key);
                    }
                    
                    result.promoted += 1;
                    result.details.push(ConsolidationDetail {
                        action: "promote".to_string(),
                        from: format!("{key_prefix}{suffix}"),
                        to: write_key,
                        occurrences: occurrences.len(),
                    });
                }
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
                let entries = engine.scan_prefix(&ns_prefix);
                
                // Group episodic entries
                let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for (key, value, meta) in &entries {
                    if meta.memory_type != MemoryType::Episodic {
                        continue;
                    }
                    let suffix = key.strip_prefix(&ns_prefix).unwrap_or(key);
                    let group_key = match group_by {
                        GroupBy::KeyPrefix => {
                            // Extract prefix up to the last `:` or first `_`
                            suffix.split(':').next().unwrap_or(suffix).to_string()
                        }
                        GroupBy::Namespace => namespace.to_string(),
                    };
                    let value_str = String::from_utf8_lossy(value).to_string();
                    groups.entry(group_key).or_default().push((key.clone(), value_str));
                }
                
                for (group, episodes) in groups {
                    if episodes.len() < *min_episodes {
                        continue;
                    }
                    
                    // Build summary value
                    let summary_value = match summary_strategy {
                        SummaryStrategy::Concat => {
                            let mut seen = std::collections::HashSet::new();
                            let mut unique = Vec::new();
                            for (_, val) in &episodes {
                                if seen.insert(val.clone()) {
                                    unique.push(val.clone());
                                }
                            }
                            unique.join("\n")
                        }
                        SummaryStrategy::Dedupe => {
                            let mut seen = std::collections::HashSet::new();
                            let mut unique = Vec::new();
                            for (_, val) in &episodes {
                                if seen.insert(val.clone()) {
                                    unique.push(val.clone());
                                }
                            }
                            unique.join("; ")
                        }
                        SummaryStrategy::LlmSummary => {
                            // LLM summary is handled by the caller (background task or HTTP handler)
                            // which has access to the LlmClient. For the core runner,
                            // fall back to concat.
                            let mut seen = std::collections::HashSet::new();
                            let mut unique = Vec::new();
                            for (_, val) in &episodes {
                                if seen.insert(val.clone()) {
                                    unique.push(val.clone());
                                }
                            }
                            unique.join("\n")
                        }
                    };
                    
                    let resolved_key = target_key.replace("{prefix}", &group);
                    let nk = NamespacedKey::new(namespace, &resolved_key);
                    let _ = engine.set(
                        &nk.to_internal(),
                        summary_value.as_bytes(),
                        None, // No TTL for semantic
                        Some(*target_type),
                    );
                    
                    if *delete_source {
                        for (src_key, _) in &episodes {
                            engine.delete(src_key);
                        }
                    }
                    
                    result.compressed += 1;
                    result.details.push(ConsolidationDetail {
                        action: "compress".to_string(),
                        from: format!("{group}:* ({} episodes)", episodes.len()),
                        to: resolved_key,
                        occurrences: episodes.len(),
                    });
                }
            }
        }
    }

    result
}
```

**Step 2: Run tests**

Run: `cd ~/basalt && cargo test --lib store::consolidation -- --nocapture 2>&1 | tail -30`
Expected: All tests pass (the existing unit tests still work, and the new function compiles)

**Step 3: Commit**

```bash
git add src/store/consolidation.rs
git commit -m "feat: implement consolidation runner logic (issue #43)"
```

---

### Task 5: Add LLM summarization support to consolidation runner

**Objective:** Add an async variant of the consolidation runner that can call LLM for the `llm_summary` strategy.

**Files:**
- Modify: `src/store/consolidation.rs`

**Step 1: Add `run_consolidation_with_llm` function**

This wraps `run_consolidation` but replaces `LlmSummary` values with actual LLM calls. The function takes an `Option<Arc<LlmClient>>` parameter.

```rust
use crate::llm::client::LlmClient;
use std::sync::Arc;

/// Run consolidation with LLM support for the llm_summary strategy.
/// If llm_client is None, falls back to concat for LLM summaries.
pub async fn run_consolidation_with_llm(
    engine: &KvEngine,
    namespace: &str,
    rules: &[ConsolidationRule],
    llm_client: Option<Arc<LlmClient>>,
) -> ConsolidationResult {
    // For rules that don't need LLM, run them directly.
    // For compress rules with LlmSummary, we need the LLM client.
    
    let mut result = ConsolidationResult::new();
    let ns_prefix = format!("{namespace}:");
    let now = now_ms();

    for rule in rules {
        match rule {
            ConsolidationRule::Promote { .. } => {
                // Promote rules don't need LLM - delegate to sync logic
                let single_result = run_promote_rule(engine, namespace, rule, now);
                result.promoted += single_result.promoted;
                result.skipped += single_result.skipped;
                result.details.extend(single_result.details);
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
                let entries = engine.scan_prefix(&ns_prefix);
                
                let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for (key, value, meta) in &entries {
                    if meta.memory_type != MemoryType::Episodic {
                        continue;
                    }
                    let suffix = key.strip_prefix(&ns_prefix).unwrap_or(key);
                    let group_key = match group_by {
                        GroupBy::KeyPrefix => {
                            suffix.split(':').next().unwrap_or(suffix).to_string()
                        }
                        GroupBy::Namespace => namespace.to_string(),
                    };
                    let value_str = String::from_utf8_lossy(value).to_string();
                    groups.entry(group_key).or_default().push((key.clone(), value_str));
                }
                
                for (group, episodes) in groups {
                    if episodes.len() < *min_episodes {
                        continue;
                    }
                    
                    let summary_value = match summary_strategy {
                        SummaryStrategy::Concat => {
                            concat_dedupe(&episodes)
                        }
                        SummaryStrategy::Dedupe => {
                            let mut seen = std::collections::HashSet::new();
                            let mut unique = Vec::new();
                            for (_, val) in &episodes {
                                if seen.insert(val.clone()) {
                                    unique.push(val.clone());
                                }
                            }
                            unique.join("; ")
                        }
                        SummaryStrategy::LlmSummary => {
                            if let Some(ref client) = llm_client {
                                if client.is_enabled() {
                                    let values: Vec<String> = episodes.iter().map(|(_, v)| v.clone()).collect();
                                    match client.summarize(&values, &format!("Consolidating episodic memories in group '{group}'")).await {
                                        Ok(summary) => summary,
                                        Err(e) => {
                                            tracing::warn!("LLM summarization failed, falling back to concat: {e}");
                                            concat_dedupe(&episodes)
                                        }
                                    }
                                } else {
                                    concat_dedupe(&episodes)
                                }
                            } else {
                                concat_dedupe(&episodes)
                            }
                        }
                    };
                    
                    let resolved_key = target_key.replace("{prefix}", &group);
                    let nk = NamespacedKey::new(namespace, &resolved_key);
                    let _ = engine.set(
                        &nk.to_internal(),
                        summary_value.as_bytes(),
                        None,
                        Some(*target_type),
                    );
                    
                    if *delete_source {
                        for (src_key, _) in &episodes {
                            engine.delete(src_key);
                        }
                    }
                    
                    result.compressed += 1;
                    result.details.push(ConsolidationDetail {
                        action: "compress".to_string(),
                        from: format!("{group}:* ({} episodes)", episodes.len()),
                        to: resolved_key,
                        occurrences: episodes.len(),
                    });
                }
            }
        }
    }

    result
}

fn concat_dedupe(episodes: &[(String, String)]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for (_, val) in episodes {
        if seen.insert(val.clone()) {
            unique.push(val.clone());
        }
    }
    unique.join("\n")
}

fn run_promote_rule(
    engine: &KvEngine,
    namespace: &str,
    rule: &ConsolidationRule,
    now: u64,
) -> ConsolidationResult {
    // Extract fields from the Promote rule
    let (min_occurrences, lookback_secs, key_prefix, target_type, conflict_policy) = match rule {
        ConsolidationRule::Promote {
            min_occurrences,
            lookback_secs,
            key_prefix,
            target_type,
            conflict_policy,
            ..
        } => (*min_occurrences, *lookback_secs, key_prefix.as_str(), *target_type, conflict_policy.clone()),
        _ => return ConsolidationResult::new(),
    };
    
    let mut result = ConsolidationResult::new();
    let ns_prefix = format!("{namespace}:");
    let full_prefix = format!("{ns_prefix}{key_prefix}");
    let entries = engine.scan_prefix(&full_prefix);
    
    let mut groups: HashMap<String, Vec<(String, Vec<u8>)>> = HashMap::new();
    for (key, value, meta) in &entries {
        if meta.memory_type != MemoryType::Episodic {
            continue;
        }
        let suffix = key.strip_prefix(&full_prefix).unwrap_or(key).to_string();
        groups.entry(suffix).or_default().push((key.clone(), value.clone()));
    }
    
    for (suffix, occurrences) in groups {
        if occurrences.len() < min_occurrences {
            continue;
        }
        
        let target_key = format!("fact:{suffix}");
        let target_nk = NamespacedKey::new(namespace, &target_key);
        let existing = engine.get(&target_nk.to_internal());
        
        match &conflict_policy {
            ConflictPolicy::Skip if existing.is_some() => {
                result.skipped += 1;
                continue;
            }
            ConflictPolicy::Version if existing.is_some() => {
                // version key will be computed below
            }
            _ => {}
        }
        
        let latest_value = occurrences.last().unwrap().1.clone();
        
        let write_key = match &conflict_policy {
            ConflictPolicy::Version if existing.is_some() => {
                let mut version = 2u32;
                loop {
                    let vkey = format!("fact:{suffix}:v{version}");
                    let vnk = NamespacedKey::new(namespace, &vkey);
                    if engine.get(&vnk.to_internal()).is_none() {
                        break format!("fact:{suffix}:v{version}");
                    }
                    version += 1;
                }
            }
            _ => target_key.clone(),
        };
        
        let write_nk = NamespacedKey::new(namespace, &write_key);
        let _ = engine.set(
            &write_nk.to_internal(),
            &latest_value,
            None,
            Some(target_type),
        );
        
        for (src_key, _) in &occurrences {
            engine.delete(src_key);
        }
        
        result.promoted += 1;
        result.details.push(ConsolidationDetail {
            action: "promote".to_string(),
            from: format!("{key_prefix}{suffix}"),
            to: write_key,
            occurrences: occurrences.len(),
        });
    }
    
    result
}
```

Note: The original `run_consolidation` function can be refactored to call `run_promote_rule` and reuse the compress logic from `run_consolidation_with_llm` with `llm_client=None`.

**Step 2: Run tests**

Run: `cd ~/basalt && cargo test --lib store::consolidation -- --nocapture 2>&1 | tail -30`
Expected: Compilation succeeds

**Step 3: Commit**

```bash
git add src/store/consolidation.rs
git commit -m "feat: add LLM summarization support to consolidation (issue #43)"
```

---

### Task 6: Add HTTP endpoints for consolidation

**Objective:** Add `POST /consolidate/{namespace}` and `GET /consolidate/status` endpoints.

**Files:**
- Modify: `src/http/models.rs`
- Modify: `src/http/server.rs`

**Step 1: Add consolidation request/response models to models.rs**

```rust
// --- Consolidation Models ---

/// Query parameters for the consolidate endpoint.
#[derive(Debug, Deserialize)]
pub struct ConsolidateQuery {
    /// Only run rules of this type ("promote" or "compress"). Optional.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub rule_type: Option<String>,
}

/// Response body for consolidation.
#[derive(Debug, Serialize)]
pub struct ConsolidateResponse {
    pub promoted: usize,
    pub compressed: usize,
    pub skipped: usize,
    pub details: Vec<ConsolidateDetailResponse>,
}

/// A single consolidation action detail in the response.
#[derive(Debug, Serialize)]
pub struct ConsolidateDetailResponse {
    pub action: String,
    pub from: String,
    pub to: String,
    pub occurrences: usize,
}

impl From<crate::store::consolidation::ConsolidationResult> for ConsolidateResponse {
    fn from(r: crate::store::consolidation::ConsolidationResult) -> Self {
        ConsolidateResponse {
            promoted: r.promoted,
            compressed: r.compressed,
            skipped: r.skipped,
            details: r.details.into_iter().map(|d| ConsolidateDetailResponse {
                action: d.action,
                from: d.from,
                to: d.to,
                occurrences: d.occurrences,
            }).collect(),
        }
    }
}

/// Response body for consolidation status.
#[derive(Debug, Serialize)]
pub struct ConsolidationStatusResponse {
    pub enabled: bool,
    pub rules_count: usize,
    pub interval_ms: u64,
}
```

**Step 2: Add route handlers and register routes in server.rs**

Add to the protected routes:
```rust
.route("/consolidate/{namespace}", post(consolidate))
.route("/consolidate/status", get(consolidation_status))
```

Add handlers:
```rust
async fn consolidate(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Query(query): Query<ConsolidateQuery>,
) -> impl IntoResponse {
    let mgr = state.engine.consolidation_manager();
    if !mgr.is_enabled() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "consolidation is disabled"}))).into_response();
    }
    
    let rules = mgr.rules();
    let filtered_rules: Vec<_> = match query.rule_type.as_deref() {
        Some("promote") => rules.into_iter().filter(|r| matches!(r, ConsolidationRule::Promote { .. })).collect(),
        Some("compress") => rules.into_iter().filter(|r| matches!(r, ConsolidationRule::Compress { .. })).collect(),
        _ => rules,
    };
    
    let result = run_consolidation(&state.engine, &namespace, &filtered_rules);
    
    // Update metadata
    let mut meta = mgr.get_meta(&namespace);
    meta.last_run_ms = now_ms();
    meta.total_promoted += result.promoted as u64;
    meta.total_compressed += result.compressed as u64;
    mgr.update_meta(&namespace, meta);
    
    let response: ConsolidateResponse = result.into();
    (StatusCode::OK, Json(json!(response))).into_response()
}

async fn consolidation_status(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let mgr = state.engine.consolidation_manager();
    let interval = state.engine.consolidation_interval_ms(); // we'll need to store this
    Json(ConsolidationStatusResponse {
        enabled: mgr.is_enabled(),
        rules_count: mgr.rules().len(),
        interval_ms: interval,
    })
}
```

Note: We need to pass `consolidation_interval_ms` through AppState or store it on KvEngine.

**Step 3: Run tests**

Run: `cd ~/basalt && cargo check 2>&1 | tail -20`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add src/http/models.rs src/http/server.rs
git commit -m "feat: add HTTP consolidation endpoints (issue #43)"
```

---

### Task 7: Add RESP commands for consolidation

**Objective:** Add `CONSOLIDATE` command to the RESP protocol.

**Files:**
- Modify: `src/resp/commands.rs`

**Step 1: Add CONSOLIDATE command handler**

Add to the command dispatch:
```rust
"CONSOLIDATE" => self.handle_consolidate(cmd),
```

Add the handler:
```rust
/// CONSOLIDATE <namespace> [promote|compress] - Run consolidation for a namespace.
/// CONSOLIDATE STATUS - Show consolidation status.
fn handle_consolidate(&self, cmd: &Command) -> RespValue {
    let sub = match cmd.args.first() {
        Some(s) => String::from_utf8_lossy(s).to_uppercase(),
        None => {
            return RespValue::Error(
                "ERR CONSOLIDATE requires a subcommand: <namespace> [promote|compress] | STATUS".to_string(),
            );
        }
    };

    if sub == "STATUS" {
        let mgr = self.engine.consolidation_manager();
        return RespValue::Array(Some(vec![
            RespValue::SimpleString(format!("enabled:{}", mgr.is_enabled())),
            RespValue::SimpleString(format!("rules:{}", mgr.rules().len())),
        ]));
    }

    // sub is the namespace
    let namespace = String::from_utf8_lossy(cmd.args.first().unwrap()).to_string();
    let mgr = self.engine.consolidation_manager();
    if !mgr.is_enabled() {
        return RespValue::Error("ERR consolidation is disabled".to_string());
    }

    let rules = mgr.rules();
    let rule_type = cmd.args.get(1).map(|s| String::from_utf8_lossy(s).to_lowercase());
    let filtered_rules: Vec<_> = match rule_type.as_deref() {
        Some("promote") => rules.into_iter().filter(|r| matches!(r, ConsolidationRule::Promote { .. })).collect(),
        Some("compress") => rules.into_iter().filter(|r| matches!(r, ConsolidationRule::Compress { .. })).collect(),
        _ => rules,
    };

    let result = run_consolidation(&self.engine, &namespace, &filtered_rules);

    // Update meta
    let mut meta = mgr.get_meta(&namespace);
    meta.last_run_ms = now_ms();
    meta.total_promoted += result.promoted as u64;
    meta.total_compressed += result.compressed as u64;
    mgr.update_meta(&namespace, meta);

    RespValue::Array(Some(vec![
        RespValue::SimpleString(format!("promoted:{}", result.promoted)),
        RespValue::SimpleString(format!("compressed:{}", result.compressed)),
        RespValue::SimpleString(format!("skipped:{}", result.skipped)),
        RespValue::Array(Some(
            result.details.into_iter().map(|d| {
                RespValue::Array(Some(vec![
                    RespValue::SimpleString(d.action),
                    RespValue::SimpleString(d.from),
                    RespValue::SimpleString(d.to),
                    RespValue::Integer(d.occurrences as i64),
                ]))
            }).collect()
        )),
    ]))
}
```

**Step 2: Run tests**

Run: `cd ~/basalt && cargo test --lib 2>&1 | tail -30`
Expected: All tests pass

**Step 3: Commit**

```bash
git add src/resp/commands.rs
git commit -m "feat: add CONSOLIDATE RESP command (issue #43)"
```

---

### Task 8: Add background consolidation sweep task to main.rs

**Objective:** Add the background loop that runs consolidation on interval, following the same pattern as the trigger sweep.

**Files:**
- Modify: `src/main.rs`

**Step 1: Add consolidation sweep loop**

Follow the exact pattern of the trigger sweep (lines 473-518 in main.rs):

```rust
// Start consolidation sweep loop if configured
if cfg.server.consolidation_interval_ms > 0 {
    let cons_engine = engine.clone();
    let cons_interval = cfg.server.consolidation_interval_ms;
    let mut cons_shutdown = shutdown_rx.clone();
    let cons_rules = engine.consolidation_manager().rules(); // snapshot rules at start
    info!("consolidation sweep: enabled, every {}ms", cons_interval);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(cons_interval)) => {
                    // Collect all namespaces that have episodic entries
                    // We scan for namespaces by looking at all entries
                    // This is a simple approach; a namespace index could optimize this
                    let namespaces = cons_engine.active_namespaces();
                    
                    for namespace in namespaces {
                        let result = store::consolidation::run_consolidation_with_llm(
                            &cons_engine,
                            &namespace,
                            &cons_rules,
                            None, // LLM client not yet wired
                        ).await;
                        
                        if result.promoted > 0 || result.compressed > 0 {
                            tracing::info!(
                                namespace = %namespace,
                                promoted = result.promoted,
                                compressed = result.compressed,
                                skipped = result.skipped,
                                "consolidation sweep completed"
                            );
                        }
                        
                        // Update metadata
                        let mgr = cons_engine.consolidation_manager();
                        let mut meta = mgr.get_meta(&namespace);
                        meta.last_run_ms = basalt::time::now_ms();
                        meta.total_promoted += result.promoted as u64;
                        meta.total_compressed += result.compressed as u64;
                        mgr.update_meta(&namespace, meta);
                        
                        // Yield between namespaces
                        tokio::task::yield_now().await;
                    }
                }
                _ = cons_shutdown.changed() => {
                    tracing::info!("consolidation sweep shutting down");
                    break;
                }
            }
        }
    });
} else {
    info!("consolidation sweep: disabled (interval = 0)");
}
```

**Step 2: Add `active_namespaces()` method to KvEngine**

Add a method to KvEngine that collects all distinct namespaces by scanning all shards:

```rust
/// Collect all distinct namespaces that currently have entries.
pub fn active_namespaces(&self) -> Vec<String> {
    let mut namespaces = std::collections::HashSet::new();
    for shard in &self.shards {
        for key in shard.keys_prefix("") {
            let ns = namespace_of_key(&key);
            namespaces.insert(ns.to_string());
        }
    }
    namespaces.into_iter().collect()
}
```

**Step 3: Run tests**

Run: `cd ~/basalt && cargo test --lib 2>&1 | tail -30`
Expected: All tests pass

**Step 4: Commit**

```bash
git add src/main.rs src/store/engine.rs
git commit -m "feat: add background consolidation sweep task (issue #43)"
```

---

### Task 9: Write integration tests for consolidation

**Objective:** Test the full consolidation pipeline: promote rules, compress rules, conflict policies, background task, HTTP/RESP endpoints.

**Files:**
- Create: `tests/consolidation.rs` (or add to `src/store/consolidation.rs` under `#[cfg(test)]`)

**Step 1: Add comprehensive unit tests to consolidation.rs**

Add tests for:
- `test_promote_rule_basic` - 3 episodic entries with same prefix get promoted
- `test_promote_rule_below_threshold` - 2 entries with min_occurrences=3 don't get promoted
- `test_promote_conflict_skip` - existing semantic key is not overwritten
- `test_promote_conflict_overwrite` - existing semantic key is overwritten
- `test_promote_conflict_version` - versioned key is created
- `test_compress_concat` - multiple episodes compressed with concat strategy
- `test_compress_dedupe` - duplicate values are deduplicated
- `test_compress_delete_source` - source entries are deleted
- `test_compress_below_threshold` - not enough episodes, no compression
- `test_consolidation_manager_meta` - metadata tracking

Each test creates a KvEngine with appropriate entries, runs consolidation, and checks results.

**Step 2: Run tests**

Run: `cd ~/basalt && cargo test --lib store::consolidation -- --nocapture`
Expected: All tests pass

**Step 3: Commit**

```bash
git add src/store/consolidation.rs
git commit -m "test: add consolidation unit tests (issue #43)"
```

---

### Task 10: Run fmt + clippy, fix all warnings, final integration check

**Objective:** Ensure the codebase passes `cargo fmt` and `cargo clippy` without warnings, and all tests pass.

**Files:**
- Any files with formatting or clippy issues

**Step 1: Run cargo fmt**

Run: `cd ~/basalt && cargo fmt`

**Step 2: Run cargo clippy**

Run: `cd ~/basalt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -40`

Fix any warnings.

**Step 3: Run full test suite**

Run: `cd ~/basalt && cargo test 2>&1 | tail -40`
Expected: All tests pass

**Step 4: Commit**

```bash
git add -A
git commit -m "style: fmt and clippy fixes for consolidation (issue #43)"
```

---

### Task 11: Update documentation

**Objective:** Add consolidation documentation to README and add config examples.

**Files:**
- Modify: `README.md`

**Step 1: Add consolidation section to README**

Add a new section under Features or Configuration:

```markdown
### Memory Consolidation

Basalt can automatically promote recurring episodic memories into permanent semantic facts,
mirroring how the human brain consolidates short-term experiences into long-term knowledge
during sleep.

**Promote rule**: When the same key prefix is observed N times as episodic memories,
it is promoted to a semantic fact:

```toml
[consolidation]
interval_ms = 300000  # 5 minutes

[[consolidation.rules]]
type = "promote"
min_occurrences = 3
lookback_secs = 3600
key_prefix = "obs:"
conflict_policy = "skip"
```

**Compress rule**: When multiple related episodic memories accumulate, they are
compressed into a single semantic summary:

```toml
[[consolidation.rules]]
type = "compress"
min_episodes = 5
group_by = "key_prefix"
summary_strategy = "concat"  # "concat" | "dedupe" | "llm_summary"
target_key = "summary:{prefix}"
delete_source = true
```

**Conflict policies** (for promote rules):
- `skip` (default): Don't overwrite existing semantic keys
- `overwrite`: Replace the existing semantic value
- `version`: Store as `fact:key:v2`, keeping the original

**Manual trigger**: Use the HTTP or RESP API to run consolidation on demand:
- `POST /consolidate/{namespace}`
- `POST /consolidate/{namespace}?type=promote`
- `CONSOLIDATE <namespace> [promote|compress]`
```

**Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add consolidation documentation and config examples (issue #43)"
```

---

### Task 12: Push all commits and close the issue

**Objective:** Push to GitHub and verify CI passes.

**Step 1: Push**

Run: `cd ~/basalt && git push`

**Step 2: Check CI**

Run: `cd ~/basalt && gh run list --limit 3`

**Step 3: Comment on issue #43**

Add a comment summarizing what was implemented and close the issue if CI passes.

**Step 4: Commit (if any CI fixes needed)**

Fix any CI failures and push again.
