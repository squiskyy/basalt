use crate::store::memory_type::MemoryType;
use crate::time::now_ms;
use papaya::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

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
}

/// Error returned when a shard has reached its entry limit.
#[derive(Debug, Clone)]
pub struct ShardFullError {
    pub max_entries: usize,
    pub current: usize,
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
        Shard {
            map: HashMap::new(),
            count: AtomicUsize::new(0),
            max_entries,
            compression_threshold,
        }
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
    /// If the key is new and the shard is at capacity, returns `Err(ShardFullError)`.
    /// The count check is approximate (TOCTOU race is acceptable for a memory guard rail).
    pub fn set(&self, key: String, entry: Entry) -> Result<(), ShardFullError> {
        let is_update = self.map.pin().get(&key).is_some();
        if !is_update && self.count.load(Ordering::Relaxed) >= self.max_entries {
            return Err(ShardFullError {
                max_entries: self.max_entries,
                current: self.count.load(Ordering::Relaxed),
            });
        }
        // Compress the value if needed (only if not already compressed)
        let entry = if !entry.compressed {
            let (value, compressed) = self.maybe_compress(entry.value);
            Entry {
                value,
                compressed,
                expires_at: entry.expires_at,
                memory_type: entry.memory_type,
                embedding: entry.embedding,
            }
        } else {
            entry
        };
        self.map.pin().insert(key, entry);
        if is_update {
            // Replace: count stays the same
        } else {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Force-set a key, ignoring shard capacity limits.
    /// Used during snapshot restore to ensure data integrity.
    pub fn set_force(&self, key: String, entry: Entry) {
        let is_update = self.map.pin().get(&key).is_some();
        // Compress the value if needed (only if not already compressed)
        let entry = if !entry.compressed {
            let (value, compressed) = self.maybe_compress(entry.value);
            Entry {
                value,
                compressed,
                expires_at: entry.expires_at,
                memory_type: entry.memory_type,
                embedding: entry.embedding,
            }
        } else {
            entry
        };
        self.map.pin().insert(key, entry);
        if !is_update {
            self.count.fetch_add(1, Ordering::Relaxed);
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
        })
    }

    /// Internal: get the raw entry (possibly compressed) from the map.
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
            Some(entry.clone())
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

    /// Scan all entries whose keys start with `prefix`, returning (key, entry) pairs.
    /// Skips expired entries (lazily removing them).
    /// Returned entries have their values decompressed.
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
                    },
                ));
            }
        }
        results
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

    /// Remove all entries whose keys start with `prefix`.
    /// Returns the number of entries removed.
    pub fn delete_prefix(&self, prefix: &str) -> usize {
        let keys_to_remove: Vec<String> = {
            let pin = self.map.pin();
            pin.iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let removed = keys_to_remove.len();
        for key in &keys_to_remove {
            self.delete(key);
        }
        removed
    }
}

impl Default for Shard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create an Entry without the compressed field (defaults to false).
    fn make_entry(value: Vec<u8>, expires_at: Option<u64>, memory_type: MemoryType) -> Entry {
        Entry {
            value,
            compressed: false,
            expires_at,
            memory_type,
            embedding: None,
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
                .set(
                    k.into(),
                    make_entry(v, None, MemoryType::Semantic),
                )
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
        shard.set("exp1".into(), make_entry(b"expired1".to_vec(), Some(1), MemoryType::Episodic)).unwrap();
        shard.set("exp2".into(), make_entry(b"expired2".to_vec(), Some(1), MemoryType::Episodic)).unwrap();
        shard.set("live".into(), make_entry(b"alive".to_vec(), None, MemoryType::Semantic)).unwrap();
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
        shard.set("key1".into(), make_entry(b"val".to_vec(), None, MemoryType::Semantic)).unwrap();
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
        shard.set("key0".into(), make_entry(b"val0".to_vec(), None, MemoryType::Semantic)).unwrap();
        shard.set("key1".into(), make_entry(b"val1".to_vec(), None, MemoryType::Semantic)).unwrap();
        // At capacity, but updating existing key should still work
        assert!(shard.set("key0".into(), make_entry(b"val0_updated".to_vec(), None, MemoryType::Semantic)).is_ok());
        assert_eq!(shard.get("key0").unwrap(), b"val0_updated");
    }

    #[test]
    fn test_shard_max_entries_delete_then_insert() {
        let shard = Shard::with_max_entries(2);
        shard.set("key0".into(), make_entry(b"val0".to_vec(), None, MemoryType::Semantic)).unwrap();
        shard.set("key1".into(), make_entry(b"val1".to_vec(), None, MemoryType::Semantic)).unwrap();
        // At capacity
        assert!(shard.set("key2".into(), make_entry(b"overflow".to_vec(), None, MemoryType::Semantic)).is_err());
        // Delete one, now insert should work
        shard.delete("key0");
        assert!(shard.set("key2".into(), make_entry(b"val2".to_vec(), None, MemoryType::Semantic)).is_ok());
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
        shard.set("ns:big".into(), make_entry(big_val.clone(), None, MemoryType::Semantic)).unwrap();

        // Insert a small value
        shard.set("ns:small".into(), make_entry(b"tiny".to_vec(), None, MemoryType::Semantic)).unwrap();

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
    fn test_shard_set_force_with_compression() {
        let shard = Shard::with_max_entries_and_threshold(100, 10);

        let big_val: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes();
        let entry = make_entry(big_val.clone(), None, MemoryType::Semantic);
        shard.set_force("force_key".into(), entry);

        // Should decompress correctly
        let result = shard.get("force_key").unwrap();
        assert_eq!(result, big_val);
    }
}
