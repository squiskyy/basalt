use crate::store::memory_type::MemoryType;
use papaya::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A single entry stored in a shard.
#[derive(Debug, Clone)]
pub struct Entry {
    pub value: Vec<u8>,
    /// UNIX timestamp in milliseconds when this entry expires. `None` means no expiry.
    pub expires_at: Option<u64>,
    pub memory_type: MemoryType,
}

impl Entry {
    /// Check if this entry has expired relative to `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.expires_at {
            Some(exp) => now_ms >= exp,
            None => false,
        }
    }
}

/// Error returned when a shard has reached its entry limit.
#[derive(Debug, Clone)]
pub struct ShardFullError {
    pub max_entries: usize,
    pub current: usize,
}

/// Returns current time in milliseconds since UNIX epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

/// Default maximum entries per shard.
const DEFAULT_MAX_ENTRIES: usize = 1_000_000;

/// A single shard of the KV store, backed by a papaya concurrent HashMap.
///
/// Sharding is used to reduce contention across keys. Each shard is independent
/// and can be accessed concurrently without blocking other shards.
pub struct Shard {
    map: HashMap<String, Entry>,
    count: AtomicUsize,
    max_entries: usize,
}

impl Shard {
    pub fn new() -> Self {
        Shard::with_max_entries(DEFAULT_MAX_ENTRIES)
    }

    /// Create a shard with a specific entry limit.
    pub fn with_max_entries(max_entries: usize) -> Self {
        Shard {
            map: HashMap::new(),
            count: AtomicUsize::new(0),
            max_entries,
        }
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
        self.map.pin().insert(key, entry);
        if !is_update {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Get the value bytes for a key, returning None if missing or expired.
    /// Expired entries are lazily removed on read.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let entry = self.get_entry(key)?;
        Some(entry.value)
    }

    /// Get the full entry for a key, returning None if missing or expired.
    /// Expired entries are lazily removed on read.
    pub fn get_entry(&self, key: &str) -> Option<Entry> {
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
                results.push((k.clone(), v.clone()));
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

    #[test]
    fn test_shard_set_get() {
        let shard = Shard::new();
        let entry = Entry {
            value: b"hello".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key1".into(), entry).unwrap();
        let val = shard.get("key1").unwrap();
        assert_eq!(val, b"hello");
    }

    #[test]
    fn test_shard_delete() {
        let shard = Shard::new();
        let entry = Entry {
            value: b"world".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key2".into(), entry).unwrap();
        assert!(shard.delete("key2"));
        assert!(!shard.delete("key2"));
        assert_eq!(shard.get("key2"), None);
    }

    #[test]
    fn test_shard_lazy_expiry() {
        let shard = Shard::new();
        let entry = Entry {
            value: b"expired".to_vec(),
            expires_at: Some(1), // already expired
            memory_type: MemoryType::Episodic,
        };
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
            shard.set(
                k.into(),
                Entry {
                    value: v,
                    expires_at: None,
                    memory_type: MemoryType::Semantic,
                },
            ).unwrap();
        }
        let results = shard.scan_prefix("ns:");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_shard_reap_expired() {
        let shard = Shard::new();
        // Insert some entries: two expired, one still live
        shard.set(
            "exp1".into(),
            Entry {
                value: b"expired1".to_vec(),
                expires_at: Some(1), // already expired
                memory_type: MemoryType::Episodic,
            },
        ).unwrap();
        shard.set(
            "exp2".into(),
            Entry {
                value: b"expired2".to_vec(),
                expires_at: Some(1),
                memory_type: MemoryType::Episodic,
            },
        ).unwrap();
        shard.set(
            "live".into(),
            Entry {
                value: b"alive".to_vec(),
                expires_at: None,
                memory_type: MemoryType::Semantic,
            },
        ).unwrap();
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
        shard.set(
            "key1".into(),
            Entry {
                value: b"val".to_vec(),
                expires_at: None,
                memory_type: MemoryType::Semantic,
            },
        ).unwrap();
        let reaped = shard.reap_expired();
        assert_eq!(reaped, 0);
        assert_eq!(shard.len(), 1);
    }

    #[test]
    fn test_shard_max_entries_rejects_at_capacity() {
        let shard = Shard::with_max_entries(3);
        for i in 0..3 {
            let entry = Entry {
                value: format!("val{i}").into_bytes(),
                expires_at: None,
                memory_type: MemoryType::Semantic,
            };
            assert!(shard.set(format!("key{i}"), entry).is_ok());
        }
        // 4th insert should fail
        let entry = Entry {
            value: b"overflow".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        let result = shard.set("key3_extra".into(), entry);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.max_entries, 3);
        assert!(err.current >= 3);
    }

    #[test]
    fn test_shard_max_entries_update_existing_key() {
        let shard = Shard::with_max_entries(2);
        let entry = Entry {
            value: b"val0".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key0".into(), entry).unwrap();
        let entry = Entry {
            value: b"val1".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key1".into(), entry).unwrap();
        // At capacity, but updating existing key should still work
        let entry = Entry {
            value: b"val0_updated".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        assert!(shard.set("key0".into(), entry).is_ok());
        assert_eq!(shard.get("key0").unwrap(), b"val0_updated");
    }

    #[test]
    fn test_shard_max_entries_delete_then_insert() {
        let shard = Shard::with_max_entries(2);
        let entry = Entry {
            value: b"val0".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key0".into(), entry).unwrap();
        let entry = Entry {
            value: b"val1".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        shard.set("key1".into(), entry).unwrap();
        // At capacity
        let entry = Entry {
            value: b"overflow".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        assert!(shard.set("key2".into(), entry).is_err());
        // Delete one, now insert should work
        shard.delete("key0");
        let entry = Entry {
            value: b"val2".to_vec(),
            expires_at: None,
            memory_type: MemoryType::Semantic,
        };
        assert!(shard.set("key2".into(), entry).is_ok());
        assert_eq!(shard.get("key2").unwrap(), b"val2");
    }
}
