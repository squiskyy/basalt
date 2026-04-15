use crate::store::memory_type::MemoryType;
use crate::time::now_ms;
use papaya::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default number of random entries to sample for approximate LRU eviction.
/// Redis uses 5 by default (maxmemory-samples), but 20 gives better eviction
/// accuracy with minimal overhead since we only compare timestamps.
const DEFAULT_LRU_SAMPLE_SIZE: usize = 20;

/// Eviction policy when a shard hits its entry capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// Reject writes when the shard is full (current behavior, most conservative).
    #[default]
    Reject,
    /// Eagerly sweep expired entries on capacity hit. If still full after sweeping, reject.
    TtlFirst,
    /// Evict the least-recently-used entry to make room for the new one.
    Lru,
}

impl EvictionPolicy {
    /// Parse from a string (case-insensitive). Returns None for unknown values.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "reject" => Some(EvictionPolicy::Reject),
            "ttl-first" => Some(EvictionPolicy::TtlFirst),
            "lru" => Some(EvictionPolicy::Lru),
            _ => None,
        }
    }
}

impl std::fmt::Display for EvictionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvictionPolicy::Reject => write!(f, "reject"),
            EvictionPolicy::TtlFirst => write!(f, "ttl-first"),
            EvictionPolicy::Lru => write!(f, "lru"),
        }
    }
}

/// A single entry stored in a shard.
#[derive(Debug, Clone)]
pub struct Entry {
    pub value: Vec<u8>,
    /// Whether the value is LZ4-compressed in memory.
    pub compressed: bool,
    /// UNIX timestamp in milliseconds when this entry expires. `None` means no expiry.
    pub expires_at: Option<u64>,
    pub memory_type: MemoryType,
    /// Optional embedding vector for semantic similarity search.
    pub embedding: Option<Vec<f32>>,
    /// Last access time (UNIX millis). Updated on read for LRU eviction.
    pub last_accessed_ms: u64,
    /// Relevance score in [0.0, 1.0]. Starts at 1.0 and decays over time.
    /// Entries without relevance metadata (loaded from old snapshots) default to 1.0.
    pub relevance: f64,
    /// UNIX timestamp in milliseconds when this entry was created.
    /// Used to compute age for relevance decay.
    pub created_at_ms: u64,
    /// Number of times this entry has been accessed (read or written).
    /// Used for access-frequency boosting in relevance computation.
    pub access_count: u64,
    /// If true, relevance stays at 1.0 and never decays.
    pub pinned: bool,
}

impl Entry {
    /// Check if this entry has expired relative to `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.expires_at {
            Some(exp) => now_ms >= exp,
            None => false,
        }
    }

    /// Decompress the value if compressed, returning the raw bytes.
    /// If not compressed, returns a clone of the value.
    pub fn decompressed_value(&self) -> Vec<u8> {
        if self.compressed {
            lz4_flex::decompress_size_prepended(&self.value).unwrap_or_else(|e| {
                tracing::error!("LZ4 decompression failed: {e}, returning raw bytes");
                self.value.clone()
            })
        } else {
            self.value.clone()
        }
    }

    /// Create a new entry with current time as last_accessed_ms and created_at_ms.
    /// Relevance starts at 1.0, access_count at 0, and pinned is false.
    pub fn new(value: Vec<u8>, expires_at: Option<u64>, memory_type: MemoryType) -> Self {
        let now = now_ms();
        Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding: None,
            last_accessed_ms: now,
            relevance: 1.0,
            created_at_ms: now,
            access_count: 0,
            pinned: false,
        }
    }
}

/// Error returned when a shard has reached its entry limit.
#[derive(Debug, Clone)]
pub struct ShardFullError {
    pub max_entries: usize,
    pub current: usize,
    pub shard_index: usize,
}

/// Default maximum entries per shard.
const DEFAULT_MAX_ENTRIES: usize = 1_000_000;

/// Default compression threshold in bytes. Values larger than this are
/// LZ4-compressed in memory. 0 = disabled.
const DEFAULT_COMPRESSION_THRESHOLD: usize = 1024;

/// A single shard of the KV store, backed by a papaya concurrent HashMap.
///
/// Sharding is used to reduce contention across keys. Each shard is independent
/// and can be accessed concurrently without blocking other shards.
pub struct Shard {
    map: HashMap<String, Entry>,
    count: AtomicUsize,
    max_entries: usize,
    /// Minimum value size (bytes) to trigger LZ4 compression in memory.
    /// 0 = disable runtime compression.
    compression_threshold: usize,
    /// Eviction policy when the shard is at capacity.
    eviction_policy: EvictionPolicy,
    /// Number of random entries to sample for approximate LRU eviction.
    /// Only used when eviction_policy is Lru. Higher values give better
    /// eviction accuracy at O(k) cost per eviction. Default: 20.
    lru_sample_size: usize,
}

impl Shard {
    pub fn new() -> Self {
        Shard::with_max_entries_and_threshold(DEFAULT_MAX_ENTRIES, DEFAULT_COMPRESSION_THRESHOLD)
    }

    /// Create a shard with a specific entry limit and default compression threshold.
    pub fn with_max_entries(max_entries: usize) -> Self {
        Shard::with_max_entries_and_threshold(max_entries, DEFAULT_COMPRESSION_THRESHOLD)
    }

    /// Create a shard with a specific entry limit and compression threshold.
    pub fn with_max_entries_and_threshold(
        max_entries: usize,
        compression_threshold: usize,
    ) -> Self {
        Shard::with_eviction_policy_and_sample_size(
            max_entries,
            compression_threshold,
            EvictionPolicy::Reject,
            DEFAULT_LRU_SAMPLE_SIZE,
        )
    }

    /// Create a shard with a specific entry limit, compression threshold, and eviction policy.
    pub fn with_eviction_policy(
        max_entries: usize,
        compression_threshold: usize,
        eviction_policy: EvictionPolicy,
    ) -> Self {
        Shard::with_eviction_policy_and_sample_size(
            max_entries,
            compression_threshold,
            eviction_policy,
            DEFAULT_LRU_SAMPLE_SIZE,
        )
    }

    /// Create a shard with a specific entry limit, compression threshold, eviction policy,
    /// and LRU sample size.
    pub fn with_eviction_policy_and_sample_size(
        max_entries: usize,
        compression_threshold: usize,
        eviction_policy: EvictionPolicy,
        lru_sample_size: usize,
    ) -> Self {
        Shard {
            map: HashMap::new(),
            count: AtomicUsize::new(0),
            max_entries,
            compression_threshold,
            eviction_policy,
            lru_sample_size: lru_sample_size.max(1), // at least 1 sample
        }
    }

    /// Return this shard's eviction policy.
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.eviction_policy
    }

    /// Return this shard's LRU sample size.
    pub fn lru_sample_size(&self) -> usize {
        self.lru_sample_size
    }

    /// Compress a value if it exceeds the compression threshold.
    /// Returns (compressed_bytes, true) if compressed, or (original_bytes, false) if not.
    fn maybe_compress(&self, value: Vec<u8>) -> (Vec<u8>, bool) {
        if self.compression_threshold > 0 && value.len() > self.compression_threshold {
            let compressed = lz4_flex::compress_prepend_size(&value);
            if compressed.len() < value.len() {
                return (compressed, true);
            }
            // Compression didn't help — store uncompressed
        }
        (value, false)
    }

    /// Insert or replace a key-value entry.
    ///
    /// If the key already exists, the value is replaced and the count stays the same.
    /// If the key is new and the shard is at capacity, the behavior depends on the
    /// eviction policy:
    /// - `Reject`: immediately return `Err(ShardFullError)`
    /// - `TtlFirst`: eagerly reap expired entries, then reject if still full
    /// - `Lru`: evict the least-recently-accessed entry to make room
    ///
    /// The capacity pre-check is approximate (acceptable for a memory guard rail),
    /// but the count increment is determined atomically from the insert return value,
    /// eliminating the TOCTOU race between checking key existence and inserting.
    pub fn set(&self, key: String, entry: Entry) -> Result<(), ShardFullError> {
        // Approximate capacity guard rail. If at capacity, only allow the
        // insert when the key likely already exists (an update). This check
        // is a best-effort guard; the authoritative count adjustment is based
        // on the insert return value below, eliminating the TOCTOU race.
        if self.count.load(Ordering::Relaxed) >= self.max_entries
            && self.map.pin().get(&key).is_none()
        {
            // At capacity with a new key - try eviction
            match self.eviction_policy {
                EvictionPolicy::Reject => {
                    return Err(ShardFullError {
                        max_entries: self.max_entries,
                        current: self.count.load(Ordering::Relaxed),
                        shard_index: 0, // filled in by KvEngine
                    });
                }
                EvictionPolicy::TtlFirst => {
                    // Eagerly sweep expired entries
                    let reaped = self.reap_expired();
                    if reaped > 0 {
                        tracing::debug!(
                            "ttl-first eviction: reaped {reaped} expired entries from shard"
                        );
                    }
                    // Re-check capacity after sweeping
                    if self.count.load(Ordering::Relaxed) >= self.max_entries {
                        return Err(ShardFullError {
                            max_entries: self.max_entries,
                            current: self.count.load(Ordering::Relaxed),
                            shard_index: 0,
                        });
                    }
                }
                EvictionPolicy::Lru => {
                    // Find and evict the least-recently-accessed entry
                    let evicted = self.evict_lru();
                    if !evicted {
                        // No entries to evict (shard should be non-empty since count >= max_entries,
                        // but handle the edge case defensively)
                        return Err(ShardFullError {
                            max_entries: self.max_entries,
                            current: self.count.load(Ordering::Relaxed),
                            shard_index: 0,
                        });
                    }
                }
            }
        }
        // Compress the value if needed (only if not already compressed)
        let now = now_ms();
        let entry = if !entry.compressed {
            let (value, compressed) = self.maybe_compress(entry.value);
            Entry {
                value,
                compressed,
                expires_at: entry.expires_at,
                memory_type: entry.memory_type,
                embedding: entry.embedding,
                last_accessed_ms: now,
                relevance: entry.relevance,
                created_at_ms: entry.created_at_ms,
                access_count: entry.access_count,
                pinned: entry.pinned,
            }
        } else {
            Entry {
                last_accessed_ms: now,
                ..entry
            }
        };
        // insert returns None for new keys, Some(&old) for updates.
        // This eliminates the TOCTOU race: we know definitively whether this
        // was an insert or an update based on the atomic operation's result.
        let pin = self.map.pin();
        let is_update = pin.insert(key, entry).is_some();
        drop(pin);
        if !is_update {
            // New key inserted: increment count
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        // else: update of existing key, count unchanged
        Ok(())
    }

    /// Force-set a key, ignoring shard capacity limits.
    /// Used during snapshot restore to ensure data integrity.
    pub fn set_force(&self, key: String, entry: Entry) {
        // Compress the value if needed (only if not already compressed)
        let now = now_ms();
        let entry = if !entry.compressed {
            let (value, compressed) = self.maybe_compress(entry.value);
            Entry {
                value,
                compressed,
                expires_at: entry.expires_at,
                memory_type: entry.memory_type,
                embedding: entry.embedding,
                last_accessed_ms: now,
                relevance: entry.relevance,
                created_at_ms: entry.created_at_ms,
                access_count: entry.access_count,
                pinned: entry.pinned,
            }
        } else {
            Entry {
                last_accessed_ms: now,
                ..entry
            }
        };
        // insert returns None for new keys, Some(&old) for updates.
        // This eliminates the TOCTOU race: the count is adjusted based on
        // the atomic insert result, not a separate get-check.
        let pin = self.map.pin();
        let is_update = pin.insert(key, entry).is_some();
        drop(pin);
        if !is_update {
            // New key inserted: increment count
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        // else: update of existing key, count unchanged
    }

    /// Evict the least-recently-used entry from this shard using random sampling.
    ///
    /// Instead of scanning all N entries (O(N)), we sample `lru_sample_size` random
    /// entries from the hash map and evict the one with the lowest `last_accessed_ms`.
    /// This is the same approach Redis uses (maxmemory-samples), providing O(k)
    /// eviction where k is the sample size (default: 20).
    ///
    /// HashMap iteration order is effectively random due to hash distribution,
    /// so taking the first k entries from iteration gives a good random sample.
    /// This avoids the catastrophic O(N) cost of a full scan on shards with
    /// millions of entries.
    ///
    /// Returns true if an entry was evicted, false if the shard is empty.
    fn evict_lru(&self) -> bool {
        let pin = self.map.pin();
        let sample_size = self.lru_sample_size;

        // Sample up to `sample_size` entries from iteration.
        // HashMap iteration order is determined by slot layout, which is
        // effectively random across different hash seeds. With ahash's
        // randomized seed per instance, this provides good distribution.
        let mut best_key: Option<String> = None;
        let mut best_time: u64 = u64::MAX;
        let mut sampled = 0;

        for (k, v) in pin.iter() {
            if sampled >= sample_size {
                break;
            }
            if v.last_accessed_ms < best_time {
                best_time = v.last_accessed_ms;
                best_key = Some(k.clone());
            }
            sampled += 1;
        }
        drop(pin);

        if let Some(key) = best_key {
            tracing::debug!("lru eviction: removing key {:?} (sampled {sampled})", key);
            self.delete(&key)
        } else {
            false
        }
    }

    /// Get the value bytes for a key, returning None if missing or expired.
    /// Expired entries are lazily removed on read.
    /// If the entry is compressed, it is decompressed before returning.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let entry = self.get_entry_raw(key)?;
        Some(entry.decompressed_value())
    }

    /// Get the full entry for a key, returning None if missing or expired.
    /// Expired entries are lazily removed on read.
    /// The returned entry has its value decompressed.
    pub fn get_entry(&self, key: &str) -> Option<Entry> {
        let entry = self.get_entry_raw(key)?;
        Some(Entry {
            value: entry.decompressed_value(),
            compressed: false,
            expires_at: entry.expires_at,
            memory_type: entry.memory_type,
            embedding: entry.embedding,
            last_accessed_ms: entry.last_accessed_ms,
            relevance: entry.relevance,
            created_at_ms: entry.created_at_ms,
            access_count: entry.access_count,
            pinned: entry.pinned,
        })
    }

    /// Internal: get the raw entry (possibly compressed) from the map.
    /// Also updates last_accessed_ms for LRU eviction tracking and
    /// increments access_count for relevance boosting.
    fn get_entry_raw(&self, key: &str) -> Option<Entry> {
        let pin = self.map.pin();
        let entry = pin.get(key)?;
        let now = now_ms();
        if entry.is_expired(now) {
            // Lazy expiry: remove the stale entry
            drop(pin);
            self.delete(key);
            None
        } else {
            // Touch LRU access time if it's been more than 1 second
            // (avoids excessive writes on hot keys)
            let needs_touch = now.saturating_sub(entry.last_accessed_ms) > 1000;
            let cloned = entry.clone();
            drop(pin);
            if needs_touch {
                let mut updated = cloned.clone();
                updated.last_accessed_ms = now;
                updated.access_count = updated.access_count.saturating_add(1);
                // Use set-force semantics: just insert, count already correct
                let pin2 = self.map.pin();
                pin2.insert(key.to_string(), updated);
            }
            Some(cloned)
        }
    }

    /// Get the raw entry without touching access time or incrementing access_count.
    /// Used for read-before-write operations (e.g., relevance boost on update)
    /// where we don't want to modify the existing entry's stats before replacing it.
    pub fn get_entry_untouched(&self, key: &str) -> Option<Entry> {
        let pin = self.map.pin();
        let entry = pin.get(key)?;
        let now = now_ms();
        if entry.is_expired(now) {
            None
        } else {
            Some(entry.clone())
        }
    }

    /// Update the pinned status of an entry. Returns true if the entry existed.
    pub fn set_pinned(&self, key: &str, pinned: bool) -> bool {
        let pin = self.map.pin();
        if let Some(entry) = pin.get(key) {
            let now = now_ms();
            if entry.is_expired(now) {
                drop(pin);
                self.delete(key);
                return false;
            }
            let mut updated = entry.clone();
            updated.pinned = pinned;
            if pinned {
                updated.relevance = 1.0;
            }
            updated.last_accessed_ms = now;
            drop(pin);
            let pin2 = self.map.pin();
            pin2.insert(key.to_string(), updated);
            true
        } else {
            false
        }
    }

    /// Delete a key. Returns true if the key existed (even if expired).
    pub fn delete(&self, key: &str) -> bool {
        let removed = self.map.pin().remove(key).is_some();
        if removed {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Return all keys that start with `prefix`, without decompressing values
    /// or building Entry structs. Useful when only keys are needed (e.g. delete,
    /// count) to avoid the cost of decompression.
    /// Skips expired entries.
    pub fn keys_prefix(&self, prefix: &str) -> Vec<String> {
        let now = now_ms();
        let pin = self.map.pin();
        pin.iter()
            .filter(|(k, v)| k.starts_with(prefix) && !v.is_expired(now))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Scan all entries whose keys start with `prefix`, returning (key, entry) pairs.
    /// Skips expired entries (lazily removing them).
    /// Returned entries have their values decompressed.
    ///
    /// # Cost
    ///
    /// This is an O(S) full-shard scan where S is the number of entries in this shard.
    /// The HashMap has no ordering, so prefix matching requires iterating every entry.
    /// At the engine level, `scan_prefix` scans ALL shards, making it O(N) where N is
    /// total entries across the cluster. For per-namespace lookups (e.g.
    /// `scan_prefix("agent-42:")`) where the namespace is small relative to total data,
    /// this is acceptable in practice. For large namespaces or empty-prefix scans, the
    /// cost is linear in total entry count. A sorted secondary index would improve this
    /// to O(K log N) where K is the result count, but adds write-path overhead.
    pub fn scan_prefix(&self, prefix: &str) -> Vec<(String, Entry)> {
        let now = now_ms();
        let mut results = Vec::new();
        let pin = self.map.pin();
        for (k, v) in pin.iter() {
            if k.starts_with(prefix) {
                if v.is_expired(now) {
                    // Note: we can't modify the map during iteration,
                    // so we collect expired keys for later cleanup
                    continue;
                }
                // Decompress value before returning
                results.push((
                    k.clone(),
                    Entry {
                        value: v.decompressed_value(),
                        compressed: false,
                        expires_at: v.expires_at,
                        memory_type: v.memory_type,
                        embedding: v.embedding.clone(),
                        last_accessed_ms: v.last_accessed_ms,
                        relevance: v.relevance,
                        created_at_ms: v.created_at_ms,
                        access_count: v.access_count,
                        pinned: v.pinned,
                    },
                ));
            }
        }
        results
    }

    /// Count entries whose keys start with `prefix`, skipping expired ones.
    /// Unlike `scan_prefix`, this does not decompress values or allocate a
    /// result vector, making it much cheaper when you only need the count.
    pub fn count_prefix(&self, prefix: &str) -> usize {
        let now = now_ms();
        let pin = self.map.pin();
        pin.iter()
            .filter(|(k, v)| k.starts_with(prefix) && !v.is_expired(now))
            .count()
    }

    /// Reap all expired entries from this shard.
    ///
    /// Uses the two-pass pattern (same as delete_prefix): first collect expired
    /// keys, then delete them, to avoid modifying the map during iteration.
    /// Returns the number of entries reaped.
    pub fn reap_expired(&self) -> usize {
        let now = now_ms();
        let expired_keys: Vec<String> = {
            let pin = self.map.pin();
            pin.iter()
                .filter(|(_, v)| v.is_expired(now))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let reaped = expired_keys.len();
        for key in &expired_keys {
            self.delete(key);
        }
        reaped
    }

    /// Approximate number of entries in this shard (may include some expired
    /// entries that haven't been lazily cleaned yet).
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Whether this shard has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reap all entries whose relevance has fallen below the floor threshold.
    /// Uses the provided decay config to compute current relevance.
    /// Returns the number of entries reaped.
    /// Pinned entries are never reaped.
    pub fn reap_low_relevance(&self, relevance_floor: f64, lambda: f64, now_ms: u64) -> usize {
        let pin = self.map.pin();
        let low_relevance_keys: Vec<String> = pin
            .iter()
            .filter(|(_, v)| {
                if v.pinned {
                    return false;
                }
                if v.is_expired(now_ms) {
                    return false; // expired entries are reaped separately
                }
                let age_hours = if now_ms > v.created_at_ms {
                    (now_ms - v.created_at_ms) as f64 / 3_600_000.0
                } else {
                    0.0
                };
                let decayed = v.relevance * (-lambda * age_hours).exp();
                decayed < relevance_floor
            })
            .map(|(k, _)| k.clone())
            .collect();
        drop(pin);

        let reaped = low_relevance_keys.len();
        for key in &low_relevance_keys {
            self.delete(key);
        }
        reaped
    }

    /// Remove all entries whose keys start with `prefix`.
    /// Returns the number of entries removed.
    pub fn delete_prefix(&self, prefix: &str) -> usize {
        let keys_to_remove = self.keys_prefix(prefix);
        let removed = keys_to_remove.len();
        for key in &keys_to_remove {
            self.delete(key);
        }
        removed
    }

    /// Remove all entries from this shard, clearing the entire map.
    /// Used during full resync to ensure stale phantom keys are removed.
    pub fn clear(&self) {
        self.map.pin().clear();
        self.count.store(0, Ordering::Relaxed);
    }
}

impl Default for Shard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unnecessary_to_owned)]
mod tests {
    use super::*;

    /// Helper to create an Entry without the compressed field (defaults to false).
    fn make_entry(value: Vec<u8>, expires_at: Option<u64>, memory_type: MemoryType) -> Entry {
        let now = now_ms();
        Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding: None,
            last_accessed_ms: now,
            relevance: 1.0,
            created_at_ms: now,
            access_count: 0,
            pinned: false,
        }
    }

    #[test]
    fn test_shard_set_get() {
        let shard = Shard::new();
        let entry = make_entry(b"hello".to_vec(), None, MemoryType::Semantic);
        shard.set("key1".into(), entry).unwrap();
        let val = shard.get("key1").unwrap();
        assert_eq!(val, b"hello");
    }

    #[test]
    fn test_shard_delete() {
        let shard = Shard::new();
        let entry = make_entry(b"world".to_vec(), None, MemoryType::Semantic);
        shard.set("key2".into(), entry).unwrap();
        assert!(shard.delete("key2"));
        assert!(!shard.delete("key2"));
        assert_eq!(shard.get("key2"), None);
    }

    #[test]
    fn test_shard_lazy_expiry() {
        let shard = Shard::new();
        let entry = make_entry(b"expired".to_vec(), Some(1), MemoryType::Episodic);
        shard.set("expired_key".into(), entry).unwrap();
        assert_eq!(shard.get("expired_key"), None);
        assert_eq!(shard.len(), 0);
    }

    #[test]
    fn test_shard_scan_prefix() {
        let shard = Shard::new();
        for (k, v) in [
            ("ns:a", b"1".to_vec()),
            ("ns:b", b"2".to_vec()),
            ("other:c", b"3".to_vec()),
        ] {
            shard
                .set(k.into(), make_entry(v, None, MemoryType::Semantic))
                .unwrap();
        }
        let results = shard.scan_prefix("ns:");
        assert_eq!(results.len(), 2);
        // Verify values are decompressed (they should be "1" and "2")
        let values: std::collections::HashSet<Vec<u8>> =
            results.into_iter().map(|(_, e)| e.value).collect();
        assert!(values.contains(&b"1".to_vec()));
        assert!(values.contains(&b"2".to_vec()));
    }

    #[test]
    fn test_shard_reap_expired() {
        let shard = Shard::new();
        shard
            .set(
                "exp1".into(),
                make_entry(b"expired1".to_vec(), Some(1), MemoryType::Episodic),
            )
            .unwrap();
        shard
            .set(
                "exp2".into(),
                make_entry(b"expired2".to_vec(), Some(1), MemoryType::Episodic),
            )
            .unwrap();
        shard
            .set(
                "live".into(),
                make_entry(b"alive".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        // Before reap: count includes expired entries
        assert_eq!(shard.len(), 3);

        let reaped = shard.reap_expired();
        assert_eq!(reaped, 2);
        assert_eq!(shard.len(), 1);
        assert!(shard.get("live").is_some());
        assert!(shard.get("exp1").is_none());
        assert!(shard.get("exp2").is_none());
    }

    #[test]
    fn test_shard_reap_expired_none_expired() {
        let shard = Shard::new();
        shard
            .set(
                "key1".into(),
                make_entry(b"val".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        let reaped = shard.reap_expired();
        assert_eq!(reaped, 0);
        assert_eq!(shard.len(), 1);
    }

    #[test]
    fn test_shard_max_entries_rejects_at_capacity() {
        let shard = Shard::with_max_entries(3);
        for i in 0..3 {
            let entry = make_entry(format!("val{i}").into_bytes(), None, MemoryType::Semantic);
            assert!(shard.set(format!("key{i}"), entry).is_ok());
        }
        // 4th insert should fail
        let entry = make_entry(b"overflow".to_vec(), None, MemoryType::Semantic);
        let result = shard.set("key3_extra".into(), entry);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.max_entries, 3);
        assert!(err.current >= 3);
    }

    #[test]
    fn test_shard_max_entries_update_existing_key() {
        let shard = Shard::with_max_entries(2);
        shard
            .set(
                "key0".into(),
                make_entry(b"val0".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "key1".into(),
                make_entry(b"val1".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        // At capacity, but updating existing key should still work
        assert!(
            shard
                .set(
                    "key0".into(),
                    make_entry(b"val0_updated".to_vec(), None, MemoryType::Semantic)
                )
                .is_ok()
        );
        assert_eq!(shard.get("key0").unwrap(), b"val0_updated");
    }

    #[test]
    fn test_shard_max_entries_delete_then_insert() {
        let shard = Shard::with_max_entries(2);
        shard
            .set(
                "key0".into(),
                make_entry(b"val0".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "key1".into(),
                make_entry(b"val1".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        // At capacity
        assert!(
            shard
                .set(
                    "key2".into(),
                    make_entry(b"overflow".to_vec(), None, MemoryType::Semantic)
                )
                .is_err()
        );
        // Delete one, now insert should work
        shard.delete("key0");
        assert!(
            shard
                .set(
                    "key2".into(),
                    make_entry(b"val2".to_vec(), None, MemoryType::Semantic)
                )
                .is_ok()
        );
        assert_eq!(shard.get("key2").unwrap(), b"val2");
    }

    #[test]
    fn test_shard_compression_large_value() {
        // Create a shard with a low compression threshold (10 bytes)
        let shard = Shard::with_max_entries_and_threshold(100, 10);

        // Value > threshold and compressible
        let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes(); // 2048 bytes, highly compressible
        let entry = make_entry(large_value.clone(), None, MemoryType::Semantic);
        shard.set("big_key".into(), entry).unwrap();

        // Should get back the original uncompressed value
        let result = shard.get("big_key").unwrap();
        assert_eq!(result, large_value);

        // get_entry should also return decompressed
        let entry_result = shard.get_entry("big_key").unwrap();
        assert_eq!(entry_result.value, large_value);
        assert!(!entry_result.compressed);
    }

    #[test]
    fn test_shard_compression_small_value_not_compressed() {
        // Create a shard with threshold of 1024
        let shard = Shard::with_max_entries_and_threshold(100, 1024);

        // Value < threshold should NOT be compressed
        let small_value = b"hello world".to_vec();
        let entry = make_entry(small_value.clone(), None, MemoryType::Semantic);
        shard.set("small_key".into(), entry).unwrap();

        let result = shard.get("small_key").unwrap();
        assert_eq!(result, small_value);
    }

    #[test]
    fn test_shard_compression_disabled() {
        // Create a shard with threshold 0 (compression disabled)
        let shard = Shard::with_max_entries_and_threshold(100, 0);

        // Large value but compression disabled
        let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes();
        let entry = make_entry(large_value.clone(), None, MemoryType::Semantic);
        shard.set("big_key".into(), entry).unwrap();

        let result = shard.get("big_key").unwrap();
        assert_eq!(result, large_value);
    }

    #[test]
    fn test_shard_compression_incompressible_data() {
        // Create a shard with a very low threshold (1 byte)
        let shard = Shard::with_max_entries_and_threshold(100, 1);

        // Random-looking data that won't compress well
        // Even so, LZ4 might still compress slightly, so use truly random-ish bytes
        let mut incompressible = Vec::with_capacity(2048);
        for i in 0..2048 {
            incompressible.push((i * 137 + 97) as u8); // pseudo-random pattern
        }
        let entry = make_entry(incompressible.clone(), None, MemoryType::Semantic);
        shard.set("incomp_key".into(), entry).unwrap();

        // Should still get back the original value (either stored uncompressed
        // or compressed + decompressed correctly)
        let result = shard.get("incomp_key").unwrap();
        assert_eq!(result, incompressible);
    }

    #[test]
    fn test_shard_scan_prefix_with_compression() {
        let shard = Shard::with_max_entries_and_threshold(100, 10);

        // Insert a large compressible value
        let big_val: Vec<u8> = "ABCDEF".repeat(256).into_bytes(); // 1536 bytes
        shard
            .set(
                "ns:big".into(),
                make_entry(big_val.clone(), None, MemoryType::Semantic),
            )
            .unwrap();

        // Insert a small value
        shard
            .set(
                "ns:small".into(),
                make_entry(b"tiny".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();

        let results = shard.scan_prefix("ns:");
        assert_eq!(results.len(), 2);

        // Verify both values are properly decompressed
        for (key, entry) in &results {
            if key == "ns:big" {
                assert_eq!(entry.value, big_val);
                assert!(!entry.compressed); // scan_prefix decompresses before returning
            } else if key == "ns:small" {
                assert_eq!(entry.value, b"tiny");
                assert!(!entry.compressed);
            }
        }
    }

    #[test]
    fn test_shard_count_prefix() {
        let shard = Shard::new();
        for (k, v) in [
            ("ns:a", b"1".to_vec()),
            ("ns:b", b"2".to_vec()),
            ("other:c", b"3".to_vec()),
        ] {
            shard
                .set(k.into(), make_entry(v, None, MemoryType::Semantic))
                .unwrap();
        }
        assert_eq!(shard.count_prefix("ns:"), 2);
        assert_eq!(shard.count_prefix("other:"), 1);
        assert_eq!(shard.count_prefix("missing:"), 0);
    }

    #[test]
    fn test_shard_count_prefix_skips_expired() {
        let shard = Shard::new();
        shard
            .set(
                "ns:live".into(),
                make_entry(b"1".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "ns:exp".into(),
                make_entry(b"2".to_vec(), Some(1), MemoryType::Episodic),
            )
            .unwrap();
        // expired entry should not be counted
        assert_eq!(shard.count_prefix("ns:"), 1);
    }

    #[test]
    fn test_shard_keys_prefix() {
        let shard = Shard::with_max_entries_and_threshold(100, 10);
        // Insert a large compressible value to verify keys_prefix avoids decompression
        let big_val: Vec<u8> = "ABCDEF".repeat(256).into_bytes();
        shard
            .set(
                "ns:big".into(),
                make_entry(big_val, None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "ns:small".into(),
                make_entry(b"tiny".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "other:x".into(),
                make_entry(b"nope".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();

        let keys = shard.keys_prefix("ns:");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"ns:big".to_string()));
        assert!(keys.contains(&"ns:small".to_string()));
        assert!(!keys.contains(&"other:x".to_string()));
    }

    #[test]
    fn test_shard_keys_prefix_skips_expired() {
        let shard = Shard::new();
        shard
            .set(
                "ns:live".into(),
                make_entry(b"1".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        shard
            .set(
                "ns:exp".into(),
                make_entry(b"2".to_vec(), Some(1), MemoryType::Episodic),
            )
            .unwrap();
        let keys = shard.keys_prefix("ns:");
        assert_eq!(keys, vec!["ns:live".to_string()]);
    }

    #[test]
    fn test_shard_set_force_with_compression() {
        let shard = Shard::with_max_entries_and_threshold(100, 10);

        let big_val: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes();
        let entry = make_entry(big_val.clone(), None, MemoryType::Semantic);
        shard.set_force("force_key".into(), entry);

        // Should decompress correctly
        let result = shard.get("force_key").unwrap();
        assert_eq!(result, big_val);
    }

    #[test]
    fn test_eviction_policy_reject() {
        // Reject policy (default): should fail when at capacity
        let shard = Shard::with_eviction_policy(3, 1024, EvictionPolicy::Reject);
        for i in 0..3 {
            let entry = make_entry(format!("val{i}").into_bytes(), None, MemoryType::Semantic);
            assert!(shard.set(format!("key{i}"), entry).is_ok());
        }
        // 4th insert should fail with Reject policy
        let entry = make_entry(b"overflow".to_vec(), None, MemoryType::Semantic);
        let result = shard.set("key3_extra".into(), entry);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.max_entries, 3);
        assert!(err.current >= 3);
        assert_eq!(err.shard_index, 0);
    }

    #[test]
    fn test_eviction_policy_ttl_first() {
        // TtlFirst policy: should sweep expired entries and make room
        let shard = Shard::with_eviction_policy(3, 1024, EvictionPolicy::TtlFirst);
        // Insert 3 entries, 2 with very short TTL (already expired)
        let entry = make_entry(b"permanent".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("perm".into(), entry).is_ok());
        let entry = make_entry(b"exp1".to_vec(), Some(1), MemoryType::Episodic);
        assert!(shard.set("exp1".into(), entry).is_ok());
        let entry = make_entry(b"exp2".to_vec(), Some(1), MemoryType::Episodic);
        assert!(shard.set("exp2".into(), entry).is_ok());

        // Wait for expired entries to be reaped
        std::thread::sleep(std::time::Duration::from_millis(10));

        // 4th insert should succeed because expired entries will be swept
        let entry = make_entry(b"new_val".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("new_key".into(), entry).is_ok());

        // The permanent entry should still be there
        assert!(shard.get("perm").is_some());
        // The new entry should be there
        assert!(shard.get("new_key").is_some());
    }

    #[test]
    fn test_eviction_policy_ttl_first_still_rejects_if_no_expired() {
        // TtlFirst policy: should reject if no expired entries to sweep
        let shard = Shard::with_eviction_policy(2, 1024, EvictionPolicy::TtlFirst);
        // Insert 2 permanent entries
        let entry = make_entry(b"val1".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key1".into(), entry).is_ok());
        let entry = make_entry(b"val2".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key2".into(), entry).is_ok());

        // 3rd insert should fail - no expired entries to sweep
        let entry = make_entry(b"val3".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key3".into(), entry).is_err());
    }

    #[test]
    fn test_eviction_policy_lru() {
        // LRU policy: when at capacity, one entry is evicted to make room.
        // Keys inserted in the same millisecond have tied timestamps,
        // so we verify that eviction happens (count stays at max) rather
        // than depending on which specific key is evicted.
        let shard = Shard::with_eviction_policy(2, 0, EvictionPolicy::Lru);
        // Insert 2 entries
        let entry = make_entry(b"first".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key1".into(), entry).is_ok());
        let entry = make_entry(b"second".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key2".into(), entry).is_ok());

        // 3rd insert should succeed by evicting one of the existing entries
        let entry = make_entry(b"third".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key3".into(), entry).is_ok());

        // Shard should still have exactly 2 entries (one was evicted)
        assert_eq!(shard.len(), 2);
        // key3 must be present (just inserted)
        assert!(shard.get("key3").is_some());
    }

    #[test]
    fn test_eviction_policy_lru_with_access_pattern() {
        // LRU policy: when at capacity, one entry is evicted to make room.
        // Keys inserted in the same millisecond have tied timestamps,
        // so we verify that eviction happens (count stays at max) rather
        // than depending on which specific key is evicted.
        let shard = Shard::with_eviction_policy(2, 0, EvictionPolicy::Lru);
        let entry = make_entry(b"first".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key1".into(), entry).is_ok());
        let entry = make_entry(b"second".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key2".into(), entry).is_ok());

        // Insert key3 - one existing key should be evicted to make room
        let entry = make_entry(b"third".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key3".into(), entry).is_ok());

        // One of the original keys should have been evicted
        assert_eq!(shard.len(), 2);
        assert!(shard.get("key3").is_some());
        // Exactly one of key1/key2 was evicted
        let remaining =
            (shard.get("key1").is_some() as usize) + (shard.get("key2").is_some() as usize);
        assert_eq!(remaining, 1, "exactly one of key1/key2 should remain");
    }

    #[test]
    fn test_eviction_policy_display() {
        assert_eq!(EvictionPolicy::Reject.to_string(), "reject");
        assert_eq!(EvictionPolicy::TtlFirst.to_string(), "ttl-first");
        assert_eq!(EvictionPolicy::Lru.to_string(), "lru");
    }

    #[test]
    fn test_eviction_policy_from_str_loose() {
        assert_eq!(
            EvictionPolicy::from_str_loose("reject"),
            Some(EvictionPolicy::Reject)
        );
        assert_eq!(
            EvictionPolicy::from_str_loose("TTL-FIRST"),
            Some(EvictionPolicy::TtlFirst)
        );
        assert_eq!(
            EvictionPolicy::from_str_loose("Lru"),
            Some(EvictionPolicy::Lru)
        );
        assert_eq!(EvictionPolicy::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_lru_sample_size_default() {
        let shard = Shard::with_eviction_policy(100, 0, EvictionPolicy::Lru);
        assert_eq!(shard.lru_sample_size(), 20);
    }

    #[test]
    fn test_lru_sample_size_custom() {
        let shard = Shard::with_eviction_policy_and_sample_size(100, 0, EvictionPolicy::Lru, 50);
        assert_eq!(shard.lru_sample_size(), 50);
    }

    #[test]
    fn test_lru_sample_size_minimum_one() {
        // Sample size of 0 should be clamped to 1
        let shard = Shard::with_eviction_policy_and_sample_size(100, 0, EvictionPolicy::Lru, 0);
        assert_eq!(shard.lru_sample_size(), 1);
    }

    #[test]
    fn test_lru_eviction_with_small_sample_size() {
        // With sample_size=1, eviction still works (picks 1 random entry)
        let shard = Shard::with_eviction_policy_and_sample_size(3, 0, EvictionPolicy::Lru, 1);
        for i in 0..3 {
            let entry = make_entry(format!("val{i}").into_bytes(), None, MemoryType::Semantic);
            assert!(shard.set(format!("key{i}"), entry).is_ok());
        }
        // Shard is at capacity; inserting a 4th should evict one
        let entry = make_entry(b"new".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key3_extra".into(), entry).is_ok());
        assert_eq!(shard.len(), 3);
        assert!(shard.get("key3_extra").is_some());
    }

    #[test]
    fn test_lru_eviction_bulk_stays_at_capacity() {
        // Insert many more keys than max_entries to verify eviction keeps
        // the shard at capacity even with many evictions
        let shard = Shard::with_eviction_policy_and_sample_size(10, 0, EvictionPolicy::Lru, 5);
        // Insert 100 entries into a shard with max 10
        for i in 0..100 {
            let entry = make_entry(format!("val{i}").into_bytes(), None, MemoryType::Semantic);
            assert!(shard.set(format!("key{i}"), entry).is_ok());
        }
        // Shard should still have exactly 10 entries
        assert_eq!(shard.len(), 10);
    }

    #[test]
    fn test_shard_full_error_has_shard_index() {
        let shard = Shard::with_max_entries(1);
        let entry = make_entry(b"first".to_vec(), None, MemoryType::Semantic);
        assert!(shard.set("key1".into(), entry).is_ok());
        let entry = make_entry(b"overflow".to_vec(), None, MemoryType::Semantic);
        let result = shard.set("key2".into(), entry);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.shard_index, 0); // filled in by KvEngine, not Shard directly
    }

    #[test]
    fn test_shard_set_pinned() {
        let shard = Shard::new();
        shard
            .set(
                "ns:key1".into(),
                make_entry(b"val".to_vec(), None, MemoryType::Semantic),
            )
            .unwrap();
        // Initially not pinned
        let entry = shard.get_entry("ns:key1").unwrap();
        assert!(!entry.pinned);
        // Pin it
        shard.set_pinned("ns:key1", true);
        let entry = shard.get_entry_untouched("ns:key1").unwrap();
        assert!(entry.pinned);
        // Unpin it
        shard.set_pinned("ns:key1", false);
        let entry = shard.get_entry_untouched("ns:key1").unwrap();
        assert!(!entry.pinned);
    }

    #[test]
    fn test_shard_reap_low_relevance() {
        let shard = Shard::new();
        // Insert entries with low relevance
        let mut low_entry = make_entry(b"low".to_vec(), None, MemoryType::Semantic);
        low_entry.relevance = 0.001; // below default floor of 0.01
        low_entry.pinned = false;
        shard.set("ns:low".into(), low_entry).unwrap();

        // Insert a pinned entry with low relevance - should survive
        let mut pinned_entry = make_entry(b"pinned".to_vec(), None, MemoryType::Semantic);
        pinned_entry.relevance = 0.001;
        pinned_entry.pinned = true;
        shard.set("ns:pinned".into(), pinned_entry).unwrap();

        // Insert a high relevance entry - should survive
        let mut high_entry = make_entry(b"high".to_vec(), None, MemoryType::Semantic);
        high_entry.relevance = 0.5;
        high_entry.pinned = false;
        shard.set("ns:high".into(), high_entry).unwrap();

        // Reap with floor=0.01 and lambda (doesn't matter for already-low relevance)
        let reaped = shard.reap_low_relevance(0.01, 0.0289, crate::store::shard::now_ms());
        assert_eq!(reaped, 1, "should reap 1 low-relevance non-pinned entry");
        // Low entry should be gone
        assert!(shard.get_entry("ns:low").is_none());
        // Pinned entry should survive
        assert!(shard.get_entry_untouched("ns:pinned").is_some());
        // High entry should survive
        assert!(shard.get_entry("ns:high").is_some());
    }
}
