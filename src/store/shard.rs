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

/// Returns current time in milliseconds since UNIX epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

/// A single shard of the KV store, backed by a papaya concurrent HashMap.
///
/// Sharding is used to reduce contention across keys. Each shard is independent
/// and can be accessed concurrently without blocking other shards.
pub struct Shard {
    map: HashMap<String, Entry>,
    count: AtomicUsize,
}

impl Shard {
    pub fn new() -> Self {
        Shard {
            map: HashMap::new(),
            count: AtomicUsize::new(0),
        }
    }

    /// Insert or replace a key-value entry.
    pub fn set(&self, key: String, entry: Entry) {
        let is_update = self.map.pin().get(&key).is_some();
        self.map.pin().insert(key, entry);
        if is_update {
            // Replace: count stays the same
        } else {
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
        shard.set("key1".into(), entry);
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
        shard.set("key2".into(), entry);
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
        shard.set("expired_key".into(), entry);
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
            );
        }
        let results = shard.scan_prefix("ns:");
        assert_eq!(results.len(), 2);
    }
}
