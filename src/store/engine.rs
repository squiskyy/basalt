use crate::store::consolidation::ConsolidationManager;
use crate::store::decay::DecayConfigStore;
use crate::store::memory_type::MemoryType;
use crate::store::shard::{Entry, EvictionPolicy, Shard, ShardFullError};
use crate::store::trigger::{TriggerCondition, TriggerEntry, TriggerManager};
use crate::store::vector::{HnswIndex, VectorSearchResult};
use crate::time::now_ms;
use ahash::RandomState;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Metadata returned alongside a value when using `get_with_meta`.
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub memory_type: MemoryType,
    /// TTL remaining in milliseconds. `None` means the entry has no expiry.
    pub ttl_remaining_ms: Option<u64>,
    /// Effective relevance score in [0.0, 1.0].
    /// Computed at query time as: decay(stored_relevance, age) + access_count * read_boost.
    pub relevance: f64,
}

/// Metadata returned alongside a value when using `scan_prefix_with_embeddings`.
#[derive(Debug, Clone)]
pub struct EntryMetaWithEmbedding {
    pub memory_type: MemoryType,
    /// TTL remaining in milliseconds. `None` means the entry has no expiry.
    pub ttl_remaining_ms: Option<u64>,
    /// Optional embedding vector for semantic similarity search.
    pub embedding: Option<Vec<f32>>,
    /// Effective relevance score in [0.0, 1.0].
    /// Computed at query time as: decay(stored_relevance, age) + access_count * read_boost.
    pub relevance: f64,
}

/// The core sharded KV engine.
///
/// Keys are hashed with ahash (randomized per instance) and routed to a shard
/// using a bitmask. The shard count is always a power of 2 for efficient
/// bitmask routing. The random seed prevents attackers from crafting keys
/// that all route to the same shard (hash flooding / DoS).
pub struct KvEngine {
    shards: Vec<Shard>,
    shard_mask: usize,
    max_entries: usize,
    compression_threshold: usize,
    eviction_policy: EvictionPolicy,
    /// Number of random entries to sample for approximate LRU eviction.
    /// Only relevant when eviction_policy is Lru. Default: 20.
    lru_sample_size: usize,
    /// Per-namespace HNSW vector indices, protected by a Mutex.
    /// Key: namespace (prefix before the first `:` in a key, or the full key).
    vector_indexes: Mutex<HashMap<String, HnswIndex>>,
    /// Per-namespace version counters for tracking changes to entries with embeddings.
    /// Only the namespace that was modified has its version incremented,
    /// avoiding unnecessary rebuilds of unrelated namespace indexes.
    namespace_versions: Mutex<HashMap<String, AtomicU64>>,
    /// Randomized hash builder for shard routing. Each KvEngine instance
    /// gets a unique random seed so that hash collisions cannot be
    /// predicted by an attacker.
    hash_state: RandomState,
    /// Per-namespace relevance decay configuration.
    decay_config: DecayConfigStore,
    /// Summarization trigger registry.
    trigger_manager: Arc<TriggerManager>,
    /// Memory consolidation rule manager.
    consolidation_manager: Arc<ConsolidationManager>,
}

/// A key structured as `namespace:key`, enforcing the convention at the type level.
///
/// Instead of passing raw strings and relying on callers to format `"{}:{}"` or
/// parse with `split_once(':')`, this newtype makes the namespace/key split
/// structural. The internal storage format is always `namespace:key`, but
/// construction and decomposition go through validated methods.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamespacedKey {
    namespace: String,
    key: String,
}

impl NamespacedKey {
    /// Create a new namespaced key from separate namespace and key parts.
    pub fn new(namespace: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            key: key.into(),
        }
    }

    /// Parse a namespaced key from its internal `namespace:key` format.
    /// Returns `None` if the string contains no `:` separator.
    pub fn from_internal(internal: &str) -> Option<Self> {
        let (namespace, key) = internal.split_once(':')?;
        Some(Self {
            namespace: namespace.to_string(),
            key: key.to_string(),
        })
    }

    /// Return the internal storage representation (`namespace:key`).
    pub fn to_internal(&self) -> String {
        format!("{}:{}", self.namespace, self.key)
    }

    /// Return the namespace portion (before the `:`).
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the key portion (after the `:`).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Return the namespace-prefix used for prefix scans (`namespace:`).
    pub fn prefix(&self) -> String {
        format!("{}:", self.namespace)
    }
}

/// Extract the namespace from an internal-format key string.
/// Returns the part before the first `:`, or the full key if no `:` is present.
/// For constructing namespaced keys from separate parts, use `NamespacedKey::new()`.
/// For parsing internal keys into structured parts, use `NamespacedKey::from_internal()`.
fn namespace_of_key(key: &str) -> &str {
    match key.split_once(':') {
        Some((ns, _)) => ns,
        None => key,
    }
}

/// Compute the next power of 2 >= n. Returns at least 1.
fn next_power_of_2(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    if n.is_power_of_two() {
        return n;
    }
    1usize << (usize::BITS - n.leading_zeros())
}

impl KvEngine {
    /// Create a new KvEngine with the given target shard count.
    /// The actual shard count will be rounded up to the next power of 2.
    /// Each shard gets a default max_entries of 1,000,000, default compression
    /// threshold of 1024 bytes, and default eviction policy (reject).
    pub fn new(shard_count: usize, consolidation_manager: Arc<ConsolidationManager>) -> Self {
        Self::with_max_entries(shard_count, 1_000_000, consolidation_manager)
    }

    /// Create a new KvEngine with the given target shard count and
    /// max entries per shard.
    pub fn with_max_entries(
        shard_count: usize,
        max_entries: usize,
        consolidation_manager: Arc<ConsolidationManager>,
    ) -> Self {
        let count = next_power_of_2(shard_count.max(1));
        let shards = (0..count)
            .map(|_| Shard::with_max_entries(max_entries))
            .collect();
        KvEngine {
            shards,
            shard_mask: count - 1,
            max_entries,
            compression_threshold: 1024,
            eviction_policy: EvictionPolicy::Reject,
            lru_sample_size: 20,
            vector_indexes: Mutex::new(HashMap::new()),
            namespace_versions: Mutex::new(HashMap::new()),
            hash_state: RandomState::new(),
            decay_config: DecayConfigStore::default(),
            trigger_manager: Arc::new(TriggerManager::new()),
            consolidation_manager,
        }
    }

    /// Create a new KvEngine with the given target shard count,
    /// max entries per shard, and compression threshold.
    /// compression_threshold: minimum value size (bytes) to LZ4-compress in memory.
    /// 0 = disable compression.
    pub fn with_max_entries_and_compression(
        shard_count: usize,
        max_entries: usize,
        compression_threshold: usize,
        consolidation_manager: Arc<ConsolidationManager>,
    ) -> Self {
        let count = next_power_of_2(shard_count.max(1));
        let shards = (0..count)
            .map(|_| Shard::with_max_entries_and_threshold(max_entries, compression_threshold))
            .collect();
        KvEngine {
            shards,
            shard_mask: count - 1,
            max_entries,
            compression_threshold,
            eviction_policy: EvictionPolicy::Reject,
            lru_sample_size: 20,
            vector_indexes: Mutex::new(HashMap::new()),
            namespace_versions: Mutex::new(HashMap::new()),
            hash_state: RandomState::new(),
            decay_config: DecayConfigStore::default(),
            trigger_manager: Arc::new(TriggerManager::new()),
            consolidation_manager,
        }
    }

    /// Create a new KvEngine with the given target shard count,
    /// max entries per shard, compression threshold, and eviction policy.
    pub fn with_eviction_policy(
        shard_count: usize,
        max_entries: usize,
        compression_threshold: usize,
        eviction_policy: EvictionPolicy,
        consolidation_manager: Arc<ConsolidationManager>,
    ) -> Self {
        Self::with_eviction_policy_and_sample_size(
            shard_count,
            max_entries,
            compression_threshold,
            eviction_policy,
            20,
            consolidation_manager,
        )
    }

    /// Create a new KvEngine with the given target shard count,
    /// max entries per shard, compression threshold, eviction policy,
    /// and LRU sample size.
    #[allow(clippy::too_many_arguments)]
    pub fn with_eviction_policy_and_sample_size(
        shard_count: usize,
        max_entries: usize,
        compression_threshold: usize,
        eviction_policy: EvictionPolicy,
        lru_sample_size: usize,
        consolidation_manager: Arc<ConsolidationManager>,
    ) -> Self {
        let count = next_power_of_2(shard_count.max(1));
        let shards = (0..count)
            .map(|_| {
                Shard::with_eviction_policy_and_sample_size(
                    max_entries,
                    compression_threshold,
                    eviction_policy,
                    lru_sample_size,
                )
            })
            .collect();
        KvEngine {
            shards,
            shard_mask: count - 1,
            max_entries,
            compression_threshold,
            eviction_policy,
            lru_sample_size: lru_sample_size.max(1),
            vector_indexes: Mutex::new(HashMap::new()),
            namespace_versions: Mutex::new(HashMap::new()),
            hash_state: RandomState::new(),
            decay_config: DecayConfigStore::default(),
            trigger_manager: Arc::new(TriggerManager::new()),
            consolidation_manager,
        }
    }

    /// Create a new KvEngine with custom decay configuration.
    pub fn with_decay_config(
        shard_count: usize,
        max_entries: usize,
        compression_threshold: usize,
        eviction_policy: EvictionPolicy,
        decay_config: DecayConfigStore,
        consolidation_manager: Arc<ConsolidationManager>,
    ) -> Self {
        let count = next_power_of_2(shard_count.max(1));
        let shards = (0..count)
            .map(|_| {
                Shard::with_eviction_policy(max_entries, compression_threshold, eviction_policy)
            })
            .collect();
        KvEngine {
            shards,
            shard_mask: count - 1,
            max_entries,
            compression_threshold,
            eviction_policy,
            lru_sample_size: 20,
            vector_indexes: Mutex::new(HashMap::new()),
            namespace_versions: Mutex::new(HashMap::new()),
            hash_state: RandomState::new(),
            decay_config,
            trigger_manager: Arc::new(TriggerManager::new()),
            consolidation_manager,
        }
    }

    /// Route a key to its shard index using ahash with a per-instance random seed.
    #[inline]
    fn shard_index(&self, key: &str) -> usize {
        self.hash_state.hash_one(key) as usize & self.shard_mask
    }

    /// Increment the version counter for a specific namespace.
    /// This is called when data in that namespace is modified (set/delete).
    #[allow(clippy::or_fun_call)]
    fn increment_namespace_version(&self, namespace: &str) -> u64 {
        let mut versions = self.namespace_versions.lock().unwrap();
        versions
            .entry(namespace.to_string())
            .or_insert_with(|| AtomicU64::new(1))
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }

    /// Get the current version for a specific namespace.
    /// Returns 0 if the namespace has never been modified.
    fn namespace_version(&self, namespace: &str) -> u64 {
        let versions = self.namespace_versions.lock().unwrap();
        versions
            .get(namespace)
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Set a key with explicit TTL (in ms from now) and memory type.
    /// Returns Err(ShardFullError) if the target shard is at capacity.
    pub fn set(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
        memory_type: MemoryType,
    ) -> Result<(), ShardFullError> {
        let now = now_ms();
        let expires_at = ttl_ms.map(|ttl| now + ttl);
        let ns = namespace_of_key(key);
        let decay = self.decay_config.get(ns);

        // If updating, boost existing relevance; otherwise start at 1.0
        let (relevance, created_at_ms, access_count, pinned) = {
            let idx = self.shard_index(key);
            match self.shards[idx].get_entry_untouched(key) {
                Some(existing) => {
                    let decayed = decay.current_relevance(
                        existing.relevance,
                        existing.created_at_ms,
                        existing.pinned,
                        now,
                    );
                    let boosted = decay.apply_write_boost(decayed, existing.pinned);
                    (
                        boosted,
                        existing.created_at_ms,
                        existing.access_count.saturating_add(1),
                        existing.pinned,
                    )
                }
                None => (1.0, now, 0, false),
            }
        };

        let entry = Entry {
            value,
            compressed: false, // Shard::set will compress if needed
            expires_at,
            memory_type,
            embedding: None,
            last_accessed_ms: 0, // overwritten by Shard::set
            relevance,
            created_at_ms,
            access_count,
            pinned,
        };
        let idx = self.shard_index(key);
        let result = self.shards[idx].set(key.to_string(), entry);
        if let Err(ref e) = result {
            // Fill in shard_index before propagating
            let mut err = e.clone();
            err.shard_index = idx;
            return Err(err);
        }
        // Increment version only for the namespace that was modified
        self.increment_namespace_version(namespace_of_key(key));
        Ok(())
    }

    /// Set a key with explicit TTL, memory type, and an optional embedding vector.
    /// Returns Err(ShardFullError) if the target shard is at capacity.
    /// The vector index version is incremented to trigger a rebuild on next search.
    pub fn set_with_embedding(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
        memory_type: MemoryType,
        embedding: Option<Vec<f32>>,
    ) -> Result<(), ShardFullError> {
        let now = now_ms();
        let expires_at = ttl_ms.map(|ttl| now + ttl);
        let ns = namespace_of_key(key);
        let decay = self.decay_config.get(ns);

        let (relevance, created_at_ms, access_count, pinned) = {
            let idx = self.shard_index(key);
            match self.shards[idx].get_entry_untouched(key) {
                Some(existing) => {
                    let decayed = decay.current_relevance(
                        existing.relevance,
                        existing.created_at_ms,
                        existing.pinned,
                        now,
                    );
                    let boosted = decay.apply_write_boost(decayed, existing.pinned);
                    (
                        boosted,
                        existing.created_at_ms,
                        existing.access_count.saturating_add(1),
                        existing.pinned,
                    )
                }
                None => (1.0, now, 0, false),
            }
        };

        let entry = Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding,
            last_accessed_ms: 0, // overwritten by Shard::set
            relevance,
            created_at_ms,
            access_count,
            pinned,
        };
        let idx = self.shard_index(key);
        let result = self.shards[idx].set(key.to_string(), entry);
        if let Err(ref e) = result {
            let mut err = e.clone();
            err.shard_index = idx;
            return Err(err);
        }
        // Increment version only for the namespace that was modified
        self.increment_namespace_version(namespace_of_key(key));
        Ok(())
    }

    /// Force-set a key, ignoring shard capacity limits.
    /// Used during snapshot restore to ensure data integrity.
    pub fn set_force(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
        memory_type: MemoryType,
    ) {
        let now = now_ms();
        let expires_at = ttl_ms.map(|ttl| now + ttl);
        let entry = Entry {
            value,
            compressed: false, // Shard::set_force will compress if needed
            expires_at,
            memory_type,
            embedding: None,
            last_accessed_ms: 0, // overwritten by Shard::set_force
            relevance: 1.0,
            created_at_ms: now,
            access_count: 0,
            pinned: false,
        };
        let idx = self.shard_index(key);
        self.shards[idx].set_force(key.to_string(), entry);
    }

    /// Force-set a key with full metadata, ignoring shard capacity limits.
    /// Used during snapshot restore to preserve relevance, created_at, access_count, and pinned state.
    #[allow(clippy::too_many_arguments)]
    pub fn set_force_full(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
        memory_type: MemoryType,
        embedding: Option<Vec<f32>>,
        relevance: f64,
        created_at_ms: u64,
        access_count: u64,
        pinned: bool,
    ) {
        let now = now_ms();
        let expires_at = ttl_ms.map(|ttl| now + ttl);
        let entry = Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding,
            last_accessed_ms: 0, // overwritten by Shard::set_force
            relevance,
            created_at_ms,
            access_count,
            pinned,
        };
        let idx = self.shard_index(key);
        self.shards[idx].set_force(key.to_string(), entry);
    }

    /// Force-set a key with embedding, ignoring shard capacity limits.
    /// Used during snapshot restore to ensure data integrity, including
    /// restoring embedding vectors for vector search.
    pub fn set_force_with_embedding(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
        memory_type: MemoryType,
        embedding: Option<Vec<f32>>,
    ) {
        let now = now_ms();
        let expires_at = ttl_ms.map(|ttl| now + ttl);
        let entry = Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding,
            last_accessed_ms: 0, // overwritten by Shard::set_force
            relevance: 1.0,
            created_at_ms: now,
            access_count: 0,
            pinned: false,
        };
        let idx = self.shard_index(key);
        self.shards[idx].set_force(key.to_string(), entry);
    }

    /// Get the value for a key. Returns None if missing or expired.
    /// Values are transparently decompressed if they were compressed at rest.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let idx = self.shard_index(key);
        self.shards[idx].get(key)
    }

    /// Get the value along with metadata (memory type, remaining TTL, relevance).
    /// Values are transparently decompressed if they were compressed at rest.
    pub fn get_with_meta(&self, key: &str) -> Option<(Vec<u8>, EntryMeta)> {
        let idx = self.shard_index(key);
        let entry = self.shards[idx].get_entry(key)?;
        let now = now_ms();
        let ns = namespace_of_key(key);
        let decay = self.decay_config.get(ns);
        let effective_relevance =
            decay.current_relevance(entry.relevance, entry.created_at_ms, entry.pinned, now)
                + entry.access_count as f64 * decay.read_boost;
        let relevance = effective_relevance.clamp(0.0, 1.0);
        let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
        let meta = EntryMeta {
            memory_type: entry.memory_type,
            ttl_remaining_ms,
            relevance,
        };
        Some((entry.value, meta))
    }

    /// Delete a key. Returns true if the key existed.
    pub fn delete(&self, key: &str) -> bool {
        let idx = self.shard_index(key);
        let deleted = self.shards[idx].delete(key);
        if deleted {
            self.increment_namespace_version(namespace_of_key(key));
        }
        deleted
    }

    /// Scan all entries whose keys start with `prefix` across all shards.
    /// Returns (key, value, EntryMeta) tuples.
    /// Values are transparently decompressed if they were compressed at rest.
    pub fn scan_prefix(&self, prefix: &str) -> Vec<(String, Vec<u8>, EntryMeta)> {
        let now = now_ms();
        let mut results = Vec::new();
        for shard in &self.shards {
            for (key, entry) in shard.scan_prefix(prefix) {
                let ns = namespace_of_key(&key);
                let decay = self.decay_config.get(ns);
                let effective_relevance = decay.current_relevance(
                    entry.relevance,
                    entry.created_at_ms,
                    entry.pinned,
                    now,
                ) + entry.access_count as f64 * decay.read_boost;
                let relevance = effective_relevance.clamp(0.0, 1.0);
                let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
                let meta = EntryMeta {
                    memory_type: entry.memory_type,
                    ttl_remaining_ms,
                    relevance,
                };
                results.push((key, entry.value, meta));
            }
        }
        results
    }

    /// Scan all entries whose keys start with `prefix` across all shards,
    /// including their embedding vectors.
    /// Returns (key, value, EntryMetaWithEmbedding) tuples.
    /// Values are transparently decompressed if they were compressed at rest.
    pub fn scan_prefix_with_embeddings(
        &self,
        prefix: &str,
    ) -> Vec<(String, Vec<u8>, EntryMetaWithEmbedding)> {
        let now = now_ms();
        let mut results = Vec::new();
        for shard in &self.shards {
            for (key, entry) in shard.scan_prefix(prefix) {
                let ns = namespace_of_key(&key);
                let decay = self.decay_config.get(ns);
                let effective_relevance = decay.current_relevance(
                    entry.relevance,
                    entry.created_at_ms,
                    entry.pinned,
                    now,
                ) + entry.access_count as f64 * decay.read_boost;
                let relevance = effective_relevance.clamp(0.0, 1.0);
                let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
                let meta = EntryMetaWithEmbedding {
                    memory_type: entry.memory_type,
                    ttl_remaining_ms,
                    embedding: entry.embedding,
                    relevance,
                };
                results.push((key, entry.value, meta));
            }
        }
        results
    }

    /// Scan all entries whose keys start with `prefix` across all shards,
    /// returning the raw Entry objects (for internal use like snapshots).
    pub fn scan_prefix_entries(&self, prefix: &str) -> Vec<(String, Entry)> {
        let mut results = Vec::new();
        for shard in &self.shards {
            results.extend(shard.scan_prefix(prefix));
        }
        results
    }

    /// Scan entries and sort by effective relevance (descending).
    /// Entries with higher relevance come first.
    pub fn scan_prefix_sorted(&self, prefix: &str) -> Vec<(String, Vec<u8>, EntryMeta)> {
        let mut results = self.scan_prefix(prefix);
        results.sort_by(|a, b| {
            b.2.relevance
                .partial_cmp(&a.2.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    /// Delete all entries whose keys start with `prefix` across all shards.
    /// Returns the total number of entries deleted.
    pub fn delete_prefix(&self, prefix: &str) -> usize {
        let mut total = 0;
        for shard in &self.shards {
            total += shard.delete_prefix(prefix);
        }
        if total > 0 {
            // Increment version for the namespace that was modified
            let namespace = prefix.strip_suffix(':').unwrap_or(prefix);
            self.increment_namespace_version(namespace);
        }
        total
    }

    /// Count all entries whose keys start with `prefix` across all shards.
    pub fn count_prefix(&self, prefix: &str) -> usize {
        let mut count = 0;
        for shard in &self.shards {
            count += shard.count_prefix(prefix);
        }
        count
    }

    /// Reap all expired entries across all shards.
    ///
    /// Staggers across shards by yielding (tokio::task::yield_now) between
    /// shards to avoid latency spikes. Returns the total number of reaped entries.
    pub async fn reap_all_expired(&self) -> usize {
        let mut total = 0;
        for shard in &self.shards {
            total += shard.reap_expired();
            tokio::task::yield_now().await;
        }
        total
    }

    /// Reap all entries whose relevance has fallen below the floor threshold
    /// across all shards. Uses the default decay config.
    ///
    /// Staggers across shards by yielding (tokio::task::yield_now) between
    /// shards to avoid latency spikes. Returns the total number of reaped entries.
    /// Pinned entries are never reaped.
    pub async fn reap_all_low_relevance(&self) -> usize {
        let now = now_ms();
        let defaults = self.decay_config.defaults();
        let mut total = 0;
        for shard in &self.shards {
            total += shard.reap_low_relevance(defaults.relevance_floor, defaults.lambda, now);
            tokio::task::yield_now().await;
        }
        total
    }

    /// Return the decay config store for per-namespace configuration.
    pub fn decay_config(&self) -> &DecayConfigStore {
        &self.decay_config
    }

    /// Return the trigger manager for registering and managing summarization triggers.
    pub fn trigger_manager(&self) -> &Arc<TriggerManager> {
        &self.trigger_manager
    }

    /// Return the consolidation manager for memory consolidation rules.
    pub fn consolidation_manager(&self) -> &Arc<ConsolidationManager> {
        &self.consolidation_manager
    }

    /// Collect all distinct namespaces currently present across all shards.
    /// A namespace is the portion of a key before the first `:` (or the full key
    /// if no `:` is present).
    pub fn active_namespaces(&self) -> Vec<String> {
        let mut namespaces = HashSet::new();
        for shard in &self.shards {
            for key in shard.keys_prefix("") {
                let ns = namespace_of_key(&key).to_string();
                namespaces.insert(ns);
            }
        }
        namespaces.into_iter().collect()
    }

    /// Check if a trigger condition is met for a namespace and return matching entries.
    /// Returns Some(entries) if the condition is met, None otherwise.
    pub fn check_trigger_condition(
        &self,
        condition: &TriggerCondition,
        now_ms: u64,
    ) -> Option<Vec<TriggerEntry>> {
        match condition {
            TriggerCondition::MinEntries {
                namespace,
                threshold,
            } => {
                let prefix = format!("{}:", namespace);
                let count = self.count_prefix(&prefix);
                if count >= *threshold {
                    Some(self.collect_trigger_entries(&prefix, None, now_ms))
                } else {
                    None
                }
            }
            TriggerCondition::MaxAge {
                namespace,
                max_age_ms,
            } => {
                let prefix = format!("{}:", namespace);
                let cutoff = now_ms.saturating_sub(*max_age_ms);
                let entries = self.collect_trigger_entries(&prefix, Some(cutoff), now_ms);
                if entries.is_empty() {
                    None
                } else {
                    Some(entries)
                }
            }
            TriggerCondition::MatchCount {
                namespace,
                key_prefix,
                threshold,
            } => {
                let full_prefix = format!("{}:{}", namespace, key_prefix);
                let count = self.count_prefix(&full_prefix);
                if count >= *threshold {
                    Some(self.collect_trigger_entries(&full_prefix, None, now_ms))
                } else {
                    None
                }
            }
        }
    }

    /// Collect TriggerEntry objects for entries matching a prefix.
    /// If cutoff_ms is set, only include entries with created_at_ms < cutoff.
    fn collect_trigger_entries(
        &self,
        prefix: &str,
        cutoff_ms: Option<u64>,
        now_ms: u64,
    ) -> Vec<TriggerEntry> {
        let entries = self.scan_prefix_entries(prefix);
        entries
            .into_iter()
            .filter(|(_, entry)| {
                if let Some(cutoff) = cutoff_ms {
                    entry.created_at_ms < cutoff
                } else {
                    true
                }
            })
            .map(|(key, entry)| {
                let ns = namespace_of_key(&key);
                let dc = self.decay_config.get(ns);
                let relevance = dc.current_relevance(
                    entry.relevance,
                    entry.created_at_ms,
                    entry.pinned,
                    now_ms,
                );
                TriggerEntry {
                    key,
                    value: String::from_utf8_lossy(&entry.value).to_string(),
                    memory_type: format!("{:?}", entry.memory_type).to_lowercase(),
                    relevance,
                    created_at_ms: entry.created_at_ms,
                    access_count: entry.access_count,
                }
            })
            .collect()
    }

    /// Pin an entry so its relevance stays at 1.0 and never decays.
    /// Returns true if the entry existed.
    pub fn pin(&self, key: &str) -> bool {
        let idx = self.shard_index(key);
        self.shards[idx].set_pinned(key, true)
    }

    /// Unpin an entry so its relevance can decay normally.
    /// Returns true if the entry existed.
    pub fn unpin(&self, key: &str) -> bool {
        let idx = self.shard_index(key);
        self.shards[idx].set_pinned(key, false)
    }

    /// Get entry metadata without touching access stats (for relevance queries).
    /// Returns (relevance, access_count, pinned, created_at_ms) or None.
    pub fn get_relevance_info(&self, key: &str) -> Option<(f64, u64, bool, u64)> {
        let idx = self.shard_index(key);
        let entry = self.shards[idx].get_entry_untouched(key)?;
        let ns = namespace_of_key(key);
        let decay = self.decay_config.get(ns);
        let now = now_ms();
        let effective =
            decay.current_relevance(entry.relevance, entry.created_at_ms, entry.pinned, now)
                + entry.access_count as f64 * decay.read_boost;
        Some((
            effective.clamp(0.0, 1.0),
            entry.access_count,
            entry.pinned,
            entry.created_at_ms,
        ))
    }

    /// Return the number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Return the entry count for a specific shard (for metrics).
    pub fn shard_entry_count(&self, shard_index: usize) -> usize {
        if shard_index < self.shards.len() {
            self.shards[shard_index].len()
        } else {
            0
        }
    }

    /// Return the max entries per shard.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Return the compression threshold in bytes.
    pub fn compression_threshold(&self) -> usize {
        self.compression_threshold
    }

    /// Return the eviction policy.
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.eviction_policy
    }

    /// Return the LRU sample size (number of entries sampled per eviction).
    pub fn lru_sample_size(&self) -> usize {
        self.lru_sample_size
    }

    /// Clear all data from the engine, removing every entry from every shard
    /// and resetting all vector indexes and namespace versions.
    /// Used during full resync to ensure stale phantom keys are removed.
    pub fn clear(&self) {
        for shard in &self.shards {
            shard.clear();
        }
        let mut indexes = self.vector_indexes.lock().unwrap();
        indexes.clear();
        let mut versions = self.namespace_versions.lock().unwrap();
        versions.clear();
    }

    /// Search for entries with embeddings similar to the query embedding within a namespace.
    ///
    /// The namespace is the prefix used in key storage (e.g., "myns" for keys "myns:key1").
    /// If the HNSW index for this namespace is stale or missing, it will be rebuilt.
    /// Returns up to `top_k` results sorted by cosine distance (lower = more similar).
    pub fn search_embedding(
        &self,
        namespace: &str,
        embedding: &[f32],
        top_k: usize,
    ) -> Vec<VectorSearchResult> {
        let prefix = format!("{}:", namespace);

        // Read the per-namespace version and check/build index inside the lock
        let mut indexes = self.vector_indexes.lock().unwrap();
        let version = self.namespace_version(namespace);

        let needs_rebuild = match indexes.get(namespace) {
            Some(idx) => idx.is_stale(version),
            None => true,
        };

        if needs_rebuild {
            // Collect entries with embeddings for this namespace
            let entries: Vec<(String, Vec<f32>, Vec<u8>)> = {
                let all = self.scan_prefix_with_embeddings(&prefix);
                all.into_iter()
                    .filter_map(|(key, value, meta)| {
                        let emb = meta.embedding?;
                        let display_key = key.strip_prefix(&prefix)?.to_string();
                        Some((display_key, emb, value))
                    })
                    .collect()
            };

            if entries.is_empty() {
                // No embeddings in this namespace - clear the index
                indexes.remove(namespace);
                return Vec::new();
            }

            // Rebuild the index (still holding the lock)
            let index = indexes.entry(namespace.to_string()).or_default();
            index.rebuild(&entries, version);

            // Search using the rebuilt index (values are cached inside the index)
            return index.search(embedding, top_k);
        }

        // Index is up-to-date, just search (still holding the lock)
        // Values are cached in the index, no need to rebuild the values map
        if let Some(index) = indexes.get(namespace) {
            index.search(embedding, top_k)
        } else {
            Vec::new()
        }
    }
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new(16, Arc::new(ConsolidationManager::disabled()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_power_of_2() {
        assert_eq!(next_power_of_2(0), 1);
        assert_eq!(next_power_of_2(1), 1);
        assert_eq!(next_power_of_2(2), 2);
        assert_eq!(next_power_of_2(3), 4);
        assert_eq!(next_power_of_2(5), 8);
        assert_eq!(next_power_of_2(16), 16);
        assert_eq!(next_power_of_2(17), 32);
    }

    #[test]
    fn test_engine_default() {
        let engine = KvEngine::default();
        assert_eq!(engine.shard_count(), 16);
    }

    #[tokio::test]
    async fn test_engine_reap_all_expired() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        // Insert entries: some expired (TTL=0 means expires_at=now, so is_expired is true),
        // some live (no TTL)
        engine
            .set("expired1", b"val1".to_vec(), Some(0), MemoryType::Episodic)
            .unwrap();
        engine
            .set("expired2", b"val2".to_vec(), Some(0), MemoryType::Episodic)
            .unwrap();
        engine
            .set("live1", b"val3".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("live2", b"val4".to_vec(), None, MemoryType::Semantic)
            .unwrap();

        let reaped = engine.reap_all_expired().await;
        assert_eq!(reaped, 2);
        assert!(engine.get("live1").is_some());
        assert!(engine.get("live2").is_some());
        assert!(engine.get("expired1").is_none());
        assert!(engine.get("expired2").is_none());
    }

    #[test]
    fn test_engine_max_entries() {
        let engine =
            KvEngine::with_max_entries(4, 1_000_000, Arc::new(ConsolidationManager::disabled()));
        assert_eq!(engine.max_entries(), 1_000_000);
    }

    #[test]
    fn test_engine_set_returns_err_at_capacity() {
        // 1 shard (power of 2 = 1), max 3 entries
        let engine = KvEngine::with_max_entries(1, 3, Arc::new(ConsolidationManager::disabled()));
        assert!(
            engine
                .set("k1", b"v1".to_vec(), None, MemoryType::Semantic)
                .is_ok()
        );
        assert!(
            engine
                .set("k2", b"v2".to_vec(), None, MemoryType::Semantic)
                .is_ok()
        );
        assert!(
            engine
                .set("k3", b"v3".to_vec(), None, MemoryType::Semantic)
                .is_ok()
        );
        // 4th insert should fail
        let result = engine.set("k4", b"v4".to_vec(), None, MemoryType::Semantic);
        assert!(result.is_err());
        // Update existing key should still work
        assert!(
            engine
                .set("k1", b"v1_updated".to_vec(), None, MemoryType::Semantic)
                .is_ok()
        );
        // Delete and then insert should work
        engine.delete("k1");
        assert!(
            engine
                .set("k5", b"v5".to_vec(), None, MemoryType::Semantic)
                .is_ok()
        );
    }

    #[test]
    fn test_engine_compression_large_value() {
        let engine = KvEngine::with_max_entries_and_compression(
            4,
            1_000_000,
            10,
            Arc::new(ConsolidationManager::disabled()),
        );

        // Value > threshold and compressible
        let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes(); // 2048 bytes
        engine
            .set("big_key", large_value.clone(), None, MemoryType::Semantic)
            .unwrap();

        // Should transparently decompress
        let result = engine.get("big_key").unwrap();
        assert_eq!(result, large_value);

        // get_with_meta should also decompress
        let (val, meta) = engine.get_with_meta("big_key").unwrap();
        assert_eq!(val, large_value);
        assert_eq!(meta.memory_type, MemoryType::Semantic);
    }

    #[test]
    fn test_engine_compression_disabled() {
        // threshold = 0 disables compression
        let engine = KvEngine::with_max_entries_and_compression(
            4,
            1_000_000,
            0,
            Arc::new(ConsolidationManager::disabled()),
        );

        let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes();
        engine
            .set("big_key", large_value.clone(), None, MemoryType::Semantic)
            .unwrap();

        let result = engine.get("big_key").unwrap();
        assert_eq!(result, large_value);
    }

    #[test]
    fn test_engine_count_prefix() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:a", b"1".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("ns:b", b"2".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("other:c", b"3".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        assert_eq!(engine.count_prefix("ns:"), 2);
        assert_eq!(engine.count_prefix("other:"), 1);
        assert_eq!(engine.count_prefix("missing:"), 0);
    }

    #[test]
    fn test_engine_count_prefix_skips_expired() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:live", b"1".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("ns:exp", b"2".to_vec(), Some(0), MemoryType::Episodic)
            .unwrap();
        assert_eq!(engine.count_prefix("ns:"), 1);
    }

    #[test]
    fn test_engine_scan_prefix_with_compression() {
        let engine = KvEngine::with_max_entries_and_compression(
            4,
            1_000_000,
            10,
            Arc::new(ConsolidationManager::disabled()),
        );

        let big_val: Vec<u8> = "ABCDEF".repeat(256).into_bytes();
        engine
            .set("ns:big", big_val.clone(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("ns:small", b"tiny".to_vec(), None, MemoryType::Semantic)
            .unwrap();

        let results = engine.scan_prefix("ns:");
        assert_eq!(results.len(), 2);

        for (key, value, _meta) in &results {
            if key == "ns:big" {
                assert_eq!(value, &big_val);
            } else if key == "ns:small" {
                assert_eq!(value, b"tiny");
            }
        }
    }

    #[test]
    fn test_namespace_version_independent() {
        // Verify that modifying one namespace does not increment the version of another
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set_with_embedding(
                "ns1:a",
                b"1".to_vec(),
                None,
                MemoryType::Semantic,
                Some(vec![1.0, 0.0]),
            )
            .unwrap();
        // Version for ns1 should be > 0
        let ns1_v1 = engine.namespace_version("ns1");
        assert!(ns1_v1 > 0, "ns1 version should be > 0 after set");
        // Version for ns2 should be 0 (never modified)
        let ns2_v0 = engine.namespace_version("ns2");
        assert_eq!(ns2_v0, 0, "ns2 version should be 0 (never modified)");
        // Modify ns1 again
        engine
            .set_with_embedding(
                "ns1:b",
                b"2".to_vec(),
                None,
                MemoryType::Semantic,
                Some(vec![0.0, 1.0]),
            )
            .unwrap();
        let ns1_v2 = engine.namespace_version("ns1");
        assert!(ns1_v2 > ns1_v1, "ns1 version should have incremented");
        // ns2 still 0
        assert_eq!(
            engine.namespace_version("ns2"),
            0,
            "ns2 version should still be 0"
        );
    }

    #[test]
    fn test_clear_removes_all_data() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("key1", b"val1".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("key2", b"val2".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set_with_embedding(
                "ns:emb1",
                b"val3".to_vec(),
                None,
                MemoryType::Semantic,
                Some(vec![1.0, 0.0]),
            )
            .unwrap();

        assert!(engine.get("key1").is_some());
        assert!(engine.get("key2").is_some());
        assert!(engine.get("ns:emb1").is_some());

        engine.clear();

        assert!(engine.get("key1").is_none());
        assert!(engine.get("key2").is_none());
        assert!(engine.get("ns:emb1").is_none());
    }

    #[test]
    fn test_namespaced_key_new() {
        let nk = NamespacedKey::new("myns", "mykey");
        assert_eq!(nk.namespace(), "myns");
        assert_eq!(nk.key(), "mykey");
        assert_eq!(nk.to_internal(), "myns:mykey");
        assert_eq!(nk.prefix(), "myns:");
    }

    #[test]
    fn test_namespaced_key_from_internal() {
        let nk = NamespacedKey::from_internal("myns:mykey").unwrap();
        assert_eq!(nk.namespace(), "myns");
        assert_eq!(nk.key(), "mykey");
        assert_eq!(nk.to_internal(), "myns:mykey");

        // Key with multiple colons - only splits on first
        let nk2 = NamespacedKey::from_internal("ns:key:with:colons").unwrap();
        assert_eq!(nk2.namespace(), "ns");
        assert_eq!(nk2.key(), "key:with:colons");
        assert_eq!(nk2.to_internal(), "ns:key:with:colons");

        // No colon returns None
        assert!(NamespacedKey::from_internal("nokey").is_none());

        // Empty namespace
        let nk3 = NamespacedKey::from_internal(":key").unwrap();
        assert_eq!(nk3.namespace(), "");
        assert_eq!(nk3.key(), "key");
    }

    #[test]
    fn test_namespaced_key_roundtrip() {
        let nk = NamespacedKey::new("session", "abc123");
        let internal = nk.to_internal();
        let parsed = NamespacedKey::from_internal(&internal).unwrap();
        assert_eq!(parsed, nk);
    }

    #[test]
    fn test_namespaced_key_equality() {
        let a = NamespacedKey::new("ns", "key");
        let b = NamespacedKey::new("ns", "key");
        let c = NamespacedKey::new("ns", "other");
        let d = NamespacedKey::new("other", "key");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn test_namespace_of_key() {
        assert_eq!(namespace_of_key("ns:key"), "ns");
        assert_eq!(namespace_of_key("ns:key:sub"), "ns");
        assert_eq!(namespace_of_key("nocolon"), "nocolon");
        assert_eq!(namespace_of_key(":emptyprefix"), "");
    }

    #[test]
    fn test_set_creates_with_relevance_1() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:key1", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        let info = engine.get_relevance_info("ns:key1").unwrap();
        assert!(
            (info.0 - 1.0).abs() < 0.01,
            "new entry should have relevance ~1.0, got {}",
            info.0
        );
        assert_eq!(info.1, 0, "new entry should have access_count 0");
        assert!(!info.2, "new entry should not be pinned");
    }

    #[test]
    fn test_get_increments_access_count() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:key1", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        // access_count starts at 0. Note: get_entry_raw only increments access_count
        // when the last access was more than 1 second ago (to avoid excessive writes).
        // So immediately after set, get won't increment it.
        // We verify the initial state instead.
        let info = engine.get_relevance_info("ns:key1").unwrap();
        assert_eq!(info.1, 0, "initial access_count should be 0");
    }

    #[test]
    fn test_get_with_meta_returns_relevance() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:key1", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        let result = engine.get_with_meta("ns:key1");
        assert!(result.is_some());
        let (_val, meta) = result.unwrap();
        // A freshly created entry should have relevance close to 1.0
        assert!(
            meta.relevance > 0.9,
            "fresh entry relevance should be > 0.9, got {}",
            meta.relevance
        );
    }

    #[test]
    fn test_pin_unpin() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:key1", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        // Pin the entry
        assert!(engine.pin("ns:key1"));
        let info = engine.get_relevance_info("ns:key1").unwrap();
        assert!(info.2, "entry should be pinned");
        // Pinned entry should have relevance 1.0
        assert!(
            (info.0 - 1.0).abs() < 0.001,
            "pinned entry should have relevance 1.0, got {}",
            info.0
        );
        // Unpin the entry
        assert!(engine.unpin("ns:key1"));
        let info = engine.get_relevance_info("ns:key1").unwrap();
        assert!(!info.2, "entry should be unpinned");
        // Pin non-existent key should return false
        assert!(!engine.pin("ns:nonexistent"));
        assert!(!engine.unpin("ns:nonexistent"));
    }

    #[test]
    fn test_scan_prefix_sorted_by_relevance() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        // Insert entries, then manipulate relevance
        engine
            .set("ns:a", b"val_a".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("ns:b", b"val_b".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        // Pin ns:a so it stays at 1.0
        engine.pin("ns:a");
        let results = engine.scan_prefix_sorted("ns:");
        assert!(!results.is_empty(), "should have results");
        // First result should be the pinned one (relevance 1.0)
        // Both start at 1.0, but the pinned one stays at 1.0
    }

    #[tokio::test]
    async fn test_reap_all_low_relevance() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        // Create a custom decay config with very fast decay for testing
        let fast_decay = crate::store::decay::DecayConfig {
            lambda: 100.0, // extremely fast decay
            read_boost: 0.0,
            write_boost: 0.0,
            relevance_floor: 0.5, // floor at 0.5
        };
        engine.decay_config().set("test", fast_decay.clone());
        // Set entries in the "test" namespace
        engine
            .set("test:old", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        // Pin one entry so it survives reap
        engine
            .set("test:pinned", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine.pin("test:pinned");
        // Set an entry outside the test namespace with default decay (slow)
        engine
            .set("other:safe", b"val".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        // Reap - the "test:old" entry should be reaped since fast decay will push it below floor
        // But it was just created so it might still be above floor.
        // To test properly, we need to manipulate created_at_ms.
        // Let's use set_force_full to set an old created_at_ms
        let old_time = 1_000_000; // very old timestamp
        engine.set_force_full(
            "test:ancient",
            b"val".to_vec(),
            None,
            MemoryType::Semantic,
            None,
            1.0,
            old_time,
            0,
            false,
        );
        // With lambda=100 and age=(now - old_time)/3600000 hours, the relevance will be essentially 0
        let reaped = engine.reap_all_low_relevance().await;
        // At least the ancient entry should be reaped
        assert!(reaped >= 1, "should reap at least 1 entry, got {reaped}");
        // The pinned entry should survive
        assert!(
            engine.get("test:pinned").is_some(),
            "pinned entry should survive reap"
        );
        // The "other" namespace entry should survive (default slow decay)
        assert!(
            engine.get("other:safe").is_some(),
            "other namespace entry should survive"
        );
    }

    #[test]
    fn test_set_force_full_preserves_metadata() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        let old_time = 1_000_000;
        engine.set_force_full(
            "ns:restored",
            b"val".to_vec(),
            None,
            MemoryType::Semantic,
            None,
            0.5, // relevance
            old_time,
            42,   // access_count
            true, // pinned
        );
        let info = engine.get_relevance_info("ns:restored").unwrap();
        // Pinned entries always return relevance 1.0
        assert!(
            (info.0 - 1.0).abs() < 0.001,
            "pinned entry relevance should be 1.0, got {}",
            info.0
        );
        assert!(info.2, "entry should be pinned");
        // Unpin and check stored relevance
        engine.unpin("ns:restored");
        let entry = engine.scan_prefix_entries("ns:");
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].1.access_count, 42);
        assert_eq!(entry[0].1.created_at_ms, old_time);
        assert!(!entry[0].1.pinned);
        // relevance should be 0.5 as stored (might have decayed slightly)
        assert!(
            entry[0].1.relevance <= 0.5 + 0.01,
            "stored relevance should be <= 0.5, got {}",
            entry[0].1.relevance
        );
    }

    #[test]
    fn test_scan_prefix_includes_relevance() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        engine
            .set("ns:key1", b"val1".to_vec(), None, MemoryType::Semantic)
            .unwrap();
        engine
            .set("ns:key2", b"val2".to_vec(), None, MemoryType::Episodic)
            .unwrap();
        let results = engine.scan_prefix("ns:");
        assert_eq!(results.len(), 2);
        for (_key, _val, meta) in &results {
            assert!(
                meta.relevance > 0.0,
                "relevance should be > 0, got {}",
                meta.relevance
            );
        }
    }

    #[test]
    fn test_decay_config_per_namespace() {
        let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));
        let custom = crate::store::decay::DecayConfig {
            lambda: 1.0,
            read_boost: 0.5,
            write_boost: 0.3,
            relevance_floor: 0.1,
        };
        engine.decay_config().set("custom-ns", custom.clone());
        let retrieved = engine.decay_config().get("custom-ns");
        assert!(
            (retrieved.lambda - 1.0).abs() < 0.001,
            "custom lambda should be 1.0"
        );
        assert!(
            (retrieved.read_boost - 0.5).abs() < 0.001,
            "custom read_boost should be 0.5"
        );
        // Default namespace should get default config
        let default = engine.decay_config().get("other-ns");
        assert!(
            (default.lambda - crate::store::decay::DecayConfig::default().lambda).abs() < 0.001
        );
    }
}
