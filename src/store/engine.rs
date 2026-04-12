use crate::store::memory_type::MemoryType;
use crate::store::shard::{Entry, Shard, ShardFullError};
use fxhash::FxHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata returned alongside a value when using `get_with_meta`.
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub memory_type: MemoryType,
    /// TTL remaining in milliseconds. `None` means the entry has no expiry.
    pub ttl_remaining_ms: Option<u64>,
}

/// The core sharded KV engine.
///
/// Keys are hashed with fxhash and routed to a shard using a bitmask.
/// The shard count is always a power of 2 for efficient bitmask routing.
pub struct KvEngine {
    shards: Vec<Shard>,
    shard_mask: usize,
    max_entries: usize,
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

/// Returns current time in milliseconds since UNIX epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

impl KvEngine {
    /// Create a new KvEngine with the given target shard count.
    /// The actual shard count will be rounded up to the next power of 2.
    /// Each shard gets a default max_entries of 1,000,000.
    pub fn new(shard_count: usize) -> Self {
        Self::with_max_entries(shard_count, 1_000_000)
    }

    /// Create a new KvEngine with the given target shard count and
    /// max entries per shard.
    pub fn with_max_entries(shard_count: usize, max_entries: usize) -> Self {
        let count = next_power_of_2(shard_count.max(1));
        let shards = (0..count).map(|_| Shard::with_max_entries(max_entries)).collect();
        KvEngine {
            shards,
            shard_mask: count - 1,
            max_entries,
        }
    }

    /// Route a key to its shard index using fxhash.
    #[inline]
    fn shard_index(&self, key: &str) -> usize {
        let mut h = FxHasher::default();
        key.hash(&mut h);
        h.finish() as usize & self.shard_mask
    }

    /// Set a key with explicit TTL (in ms from now) and memory type.
    /// Returns Err(ShardFullError) if the target shard is at capacity.
    pub fn set(&self, key: &str, value: Vec<u8>, ttl_ms: Option<u64>, memory_type: MemoryType) -> Result<(), ShardFullError> {
        let expires_at = ttl_ms.map(|ttl| now_ms() + ttl);
        let entry = Entry {
            value,
            expires_at,
            memory_type,
        };
        let idx = self.shard_index(key);
        self.shards[idx].set(key.to_string(), entry)
    }

    /// Force-set a key, ignoring shard capacity limits.
    /// Used during snapshot restore to ensure data integrity.
    pub fn set_force(&self, key: &str, value: Vec<u8>, ttl_ms: Option<u64>, memory_type: MemoryType) {
        let expires_at = ttl_ms.map(|ttl| now_ms() + ttl);
        let entry = Entry {
            value,
            expires_at,
            memory_type,
        };
        let idx = self.shard_index(key);
        self.shards[idx].set_force(key.to_string(), entry);
    }

    /// Get the value for a key. Returns None if missing or expired.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let idx = self.shard_index(key);
        self.shards[idx].get(key)
    }

    /// Get the value along with metadata (memory type and remaining TTL).
    pub fn get_with_meta(&self, key: &str) -> Option<(Vec<u8>, EntryMeta)> {
        let idx = self.shard_index(key);
        let entry = self.shards[idx].get_entry(key)?;
        let now = now_ms();
        let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
        let meta = EntryMeta {
            memory_type: entry.memory_type,
            ttl_remaining_ms,
        };
        Some((entry.value, meta))
    }

    /// Delete a key. Returns true if the key existed.
    pub fn delete(&self, key: &str) -> bool {
        let idx = self.shard_index(key);
        self.shards[idx].delete(key)
    }

    /// Scan all entries whose keys start with `prefix` across all shards.
    /// Returns (key, value, EntryMeta) tuples.
    pub fn scan_prefix(&self, prefix: &str) -> Vec<(String, Vec<u8>, EntryMeta)> {
        let now = now_ms();
        let mut results = Vec::new();
        for shard in &self.shards {
            for (key, entry) in shard.scan_prefix(prefix) {
                let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
                let meta = EntryMeta {
                    memory_type: entry.memory_type,
                    ttl_remaining_ms,
                };
                results.push((key, entry.value, meta));
            }
        }
        results
    }

    /// Delete all entries whose keys start with `prefix` across all shards.
    /// Returns the total number of entries deleted.
    pub fn delete_prefix(&self, prefix: &str) -> usize {
        let mut total = 0;
        for shard in &self.shards {
            total += shard.delete_prefix(prefix);
        }
        total
    }

    /// Count all entries whose keys start with `prefix` across all shards.
    pub fn count_prefix(&self, prefix: &str) -> usize {
        let mut count = 0;
        for shard in &self.shards {
            count += shard.scan_prefix(prefix).len();
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

    /// Return the number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Return the max entries per shard.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new(16)
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
        let engine = KvEngine::new(4);
        // Insert entries: some expired (TTL=0 means expires_at=now, so is_expired is true),
        // some live (no TTL)
        engine.set("expired1", b"val1".to_vec(), Some(0), MemoryType::Episodic).unwrap();
        engine.set("expired2", b"val2".to_vec(), Some(0), MemoryType::Episodic).unwrap();
        engine.set("live1", b"val3".to_vec(), None, MemoryType::Semantic).unwrap();
        engine.set("live2", b"val4".to_vec(), None, MemoryType::Semantic).unwrap();

        let reaped = engine.reap_all_expired().await;
        assert_eq!(reaped, 2);
        assert!(engine.get("live1").is_some());
        assert!(engine.get("live2").is_some());
        assert!(engine.get("expired1").is_none());
        assert!(engine.get("expired2").is_none());
    }

    #[test]
    fn test_engine_max_entries() {
        let engine = KvEngine::with_max_entries(4, 1_000_000);
        assert_eq!(engine.max_entries(), 1_000_000);
    }

    #[test]
    fn test_engine_set_returns_err_at_capacity() {
        // 1 shard (power of 2 = 1), max 3 entries
        let engine = KvEngine::with_max_entries(1, 3);
        assert!(engine.set("k1", b"v1".to_vec(), None, MemoryType::Semantic).is_ok());
        assert!(engine.set("k2", b"v2".to_vec(), None, MemoryType::Semantic).is_ok());
        assert!(engine.set("k3", b"v3".to_vec(), None, MemoryType::Semantic).is_ok());
        // 4th insert should fail
        let result = engine.set("k4", b"v4".to_vec(), None, MemoryType::Semantic);
        assert!(result.is_err());
        // Update existing key should still work
        assert!(engine.set("k1", b"v1_updated".to_vec(), None, MemoryType::Semantic).is_ok());
        // Delete and then insert should work
        engine.delete("k1");
        assert!(engine.set("k5", b"v5".to_vec(), None, MemoryType::Semantic).is_ok());
    }
}
