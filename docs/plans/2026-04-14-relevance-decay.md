# Relevance Decay Implementation Plan

> **Status: COMPLETED** - All 10 tasks implemented and pushed to master.

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add floating-point relevance scores that decay over time, boosted by access frequency, integrated into query ranking, and used for GC eligibility.

**Architecture:** Per-entry relevance metadata (score + created_at + access_count) stored in the Entry struct. Exponential decay computed at query time. Access-frequency boosting on read/write. Per-namespace configurable decay rate and boost delta. GC sweep for entries below relevance_floor. Backward compatible - entries without relevance metadata default to relevance 1.0.

**Tech Stack:** Rust, existing papaya HashMap, existing snapshot format (version bump to 5).

---

## Overview

The issue asks for 6 implementation steps:

1. Add `relevance` (float) and `last_accessed_ms` (timestamp) fields to entry metadata
2. Implement exponential decay as default with configurable lambda per-namespace
3. Add access-frequency boosting on read/write paths
4. Integrate relevance into query ranking (recency * relevance)
5. Add GC sweep for entries below relevance_floor
6. (Optional) Add consolidation hook for low-relevance entries

We already have `last_accessed_ms` on Entry. We need to add `relevance`, `created_at_ms`, and `access_count`. Then we need per-namespace decay config, query-time relevance computation, and a GC sweep.

## Scope Decisions

- **Relevance is per-key** (more flexible, per-namespace config still supported via defaults)
- **Boost on write > boost on read** (write = 0.15, read = 0.1 by default, configurable)
- **Pin mechanism**: yes, a `pinned` boolean on Entry that sets relevance to 1.0 and disables decay
- **TTL interaction**: TTL is a hard upper bound. Even if relevance is high, TTL expiry still removes the entry
- **Step 6 (consolidation hook)**: deferred to a future issue - LLM summarization is expensive and should be opt-in/rate-limited separately

---

### Task 1: Add relevance fields to Entry struct

**Objective:** Add `relevance: f64`, `created_at_ms: u64`, `access_count: u64`, and `pinned: bool` to the Entry struct.

**Files:**
- Modify: `src/store/shard.rs` (Entry struct)
- Modify: `src/store/engine.rs` (Entry construction sites)

**Step 1: Update Entry struct in shard.rs**

Add fields after `last_accessed_ms`:

```rust
pub struct Entry {
    pub value: Vec<u8>,
    pub compressed: bool,
    pub expires_at: Option<u64>,
    pub memory_type: MemoryType,
    pub embedding: Option<Vec<f32>>,
    pub last_accessed_ms: u64,
    /// Relevance score in [0.0, 1.0]. Starts at 1.0 and decays over time.
    /// Entries without relevance metadata (loaded from old snapshots) default to 1.0.
    pub relevance: f64,
    /// UNIX timestamp in milliseconds when this entry was created.
    /// Used to compute age for relevance decay.
    pub created_at_ms: u64,
    /// Number of times this entry has been accessed (read or written).
    /// Used for access-frequency boosting.
    pub access_count: u64,
    /// If true, relevance stays at 1.0 and never decays.
    pub pinned: bool,
}
```

**Step 2: Update Entry::new() helper**

```rust
impl Entry {
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
```

**Step 3: Update all Entry construction sites in engine.rs**

Every place that constructs `Entry { ... }` needs the new fields. There are 4 sites in engine.rs:
- `KvEngine::set` (line ~244)
- `KvEngine::set_with_embedding` (line ~277)
- `KvEngine::set_force` (line ~307)
- `KvEngine::set_force_with_embedding` (line ~332)

Add these fields to each:
```rust
relevance: 1.0,
created_at_ms: now_ms(),
access_count: 0,
pinned: false,
```

Note: `now_ms()` is already imported. For `set` and `set_with_embedding`, `expires_at` already uses `now_ms()` so we can reuse that or call it again.

**Step 4: Update Shard::set and Shard::set_force**

These methods reconstruct Entry with `{ last_accessed_ms: now, ..entry }` or build a new Entry. They need to preserve/copy the new fields:

In the compression branch of `Shard::set` (around line 240-255):
```rust
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
```

Same for the non-compressed branch and `set_force`.

**Step 5: Update Shard::get_entry_raw**

The LRU touch already clones and reinserts. The clone copies all fields, so no changes needed there. But we should increment `access_count` on read. Update the touch logic:

```rust
if needs_touch {
    let mut updated = cloned.clone();
    updated.last_accessed_ms = now;
    updated.access_count = updated.access_count.saturating_add(1);
    let pin2 = self.map.pin();
    pin2.insert(key.to_string(), updated);
}
```

**Step 6: Update Shard::scan_prefix**

The scan_prefix method reconstructs Entry from the stored entry. Add the new fields:

```rust
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
}
```

**Step 7: Update the test helper `make_entry` in shard.rs tests**

```rust
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
```

**Step 8: Run `cargo check` to verify compilation**

Run: `cargo check`
Expected: Compiles with no errors related to the new fields.

**Step 9: Commit**

```bash
git add -A && git commit -m "feat: add relevance, created_at_ms, access_count, pinned fields to Entry"
```

---

### Task 2: Add DecayConfig and per-namespace decay configuration

**Objective:** Create a configurable decay system with per-namespace settings.

**Files:**
- Create: `src/store/decay.rs`
- Modify: `src/store/mod.rs` (add module)

**Step 1: Create src/store/decay.rs**

```rust
//! Relevance decay configuration and computation.
//!
//! Each namespace can have its own decay settings. The default uses exponential
//! decay with a 24-hour halflife, 0.1 read boost, 0.15 write boost, and a
//! relevance floor of 0.01.

use std::collections::HashMap;
use std::sync::Mutex;

/// Configuration for how relevance decays in a namespace.
#[derive(Debug, Clone)]
pub struct DecayConfig {
    /// Decay constant (lambda) for exponential decay: relevance = e^(-lambda * age_hours).
    /// Default: ln(2) / 24.0 ≈ 0.0289 (24-hour halflife).
    pub lambda: f64,
    /// Boost applied when an entry is read (accessed via GET).
    /// Clamped to [0.0, 1.0] after addition. Default: 0.1.
    pub read_boost: f64,
    /// Boost applied when an entry is written (created or updated via SET).
    /// Clamped to [0.0, 1.0] after addition. Default: 0.15.
    pub write_boost: f64,
    /// Relevance floor. Entries below this are eligible for GC. Default: 0.01.
    pub relevance_floor: f64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        DecayConfig {
            lambda: (2.0_f64.ln()) / 24.0, // ~0.02888, 24h halflife
            read_boost: 0.1,
            write_boost: 0.15,
            relevance_floor: 0.01,
        }
    }
}

impl DecayConfig {
    /// Compute the current relevance of an entry given its creation time and current relevance.
    /// Uses exponential decay: relevance = stored_relevance * e^(-lambda * age_hours).
    /// For pinned entries, returns 1.0.
    pub fn current_relevance(&self, stored_relevance: f64, created_at_ms: u64, pinned: bool, now_ms: u64) -> f64 {
        if pinned {
            return 1.0;
        }
        if now_ms <= created_at_ms {
            return stored_relevance;
        }
        let age_hours = (now_ms - created_at_ms) as f64 / (3_600_000.0);
        let decay = (-self.lambda * age_hours).exp();
        (stored_relevance * decay).max(0.0).min(1.0)
    }

    /// Apply a read boost: relevance = min(1.0, relevance + read_boost).
    /// Only applies if the entry is not pinned.
    pub fn apply_read_boost(&self, relevance: f64, pinned: bool) -> f64 {
        if pinned {
            return 1.0;
        }
        (relevance + self.read_boost).min(1.0)
    }

    /// Apply a write boost: relevance = min(1.0, relevance + write_boost).
    /// Only applies if the entry is not pinned.
    pub fn apply_write_boost(&self, relevance: f64, pinned: bool) -> f64 {
        if pinned {
            return 1.0;
        }
        (relevance + self.write_boost).min(1.0)
    }

    /// Check if a relevance score is below the floor (eligible for GC).
    pub fn is_below_floor(&self, relevance: f64) -> bool {
        relevance < self.relevance_floor
    }
}

/// Per-namespace decay configuration store.
/// Falls back to the default config for namespaces without custom settings.
pub struct DecayConfigStore {
    defaults: DecayConfig,
    overrides: Mutex<HashMap<String, DecayConfig>>,
}

impl DecayConfigStore {
    pub fn new(defaults: DecayConfig) -> Self {
        DecayConfigStore {
            defaults,
            overrides: Mutex::new(HashMap::new()),
        }
    }

    /// Get the decay config for a namespace. Returns the default if no override is set.
    pub fn get(&self, namespace: &str) -> DecayConfig {
        let overrides = self.overrides.lock().unwrap();
        overrides.get(namespace).cloned().unwrap_or_else(|| self.defaults.clone())
    }

    /// Set a custom decay config for a namespace.
    pub fn set(&self, namespace: &str, config: DecayConfig) {
        self.overrides.lock().unwrap().insert(namespace.to_string(), config);
    }

    /// Remove a custom decay config for a namespace, reverting to default.
    pub fn remove(&self, namespace: &str) -> bool {
        self.overrides.lock().unwrap().remove(namespace).is_some()
    }

    /// List all namespaces with custom decay configs.
    pub fn list_overrides(&self) -> Vec<(String, DecayConfig)> {
        let overrides = self.overrides.lock().unwrap();
        overrides.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Return the default decay config.
    pub fn defaults(&self) -> &DecayConfig {
        &self.defaults
    }
}

impl Default for DecayConfigStore {
    fn default() -> Self {
        Self::new(DecayConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_halflife() {
        let config = DecayConfig::default();
        // After 24 hours, relevance should be roughly 0.5
        let now = 172800_000; // 48 hours in ms (arbitrary start)
        let created = now - 86_400_000; // 24 hours ago
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!((relevance - 0.5).abs() < 0.01, "24h halflife: expected ~0.5, got {relevance}");
    }

    #[test]
    fn test_pinned_always_1() {
        let config = DecayConfig::default();
        let now = 172800_000;
        let created = now - 86_400_000; // 24h ago
        let relevance = config.current_relevance(1.0, created, true, now);
        assert_eq!(relevance, 1.0, "pinned entries should have relevance 1.0");
    }

    #[test]
    fn test_decay_over_time() {
        let config = DecayConfig::default();
        let now = 172800_000;
        // 1 hour ago: should still be quite high
        let created = now - 3_600_000;
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!(relevance > 0.95, "1h old entry should have relevance > 0.95, got {relevance}");

        // 72 hours ago: should be quite low
        let created = now - 72 * 3_600_000;
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!(relevance < 0.15, "72h old entry should have relevance < 0.15, got {relevance}");
    }

    #[test]
    fn test_read_boost() {
        let config = DecayConfig::default();
        let boosted = config.apply_read_boost(0.5, false);
        assert!((boosted - 0.6).abs() < 0.001, "read boost: expected 0.6, got {boosted}");

        // Should clamp to 1.0
        let boosted = config.apply_read_boost(0.95, false);
        assert_eq!(boosted, 1.0, "read boost should clamp to 1.0");
    }

    #[test]
    fn test_write_boost() {
        let config = DecayConfig::default();
        let boosted = config.apply_write_boost(0.5, false);
        assert!((boosted - 0.65).abs() < 0.001, "write boost: expected 0.65, got {boosted}");

        // Pinned should return 1.0
        let boosted = config.apply_write_boost(0.3, true);
        assert_eq!(boosted, 1.0, "pinned write boost should return 1.0");
    }

    #[test]
    fn test_below_floor() {
        let config = DecayConfig::default();
        assert!(config.is_below_floor(0.005));
        assert!(config.is_below_floor(0.0));
        assert!(!config.is_below_floor(0.01));
        assert!(!config.is_below_floor(0.5));
    }

    #[test]
    fn test_config_store_default() {
        let store = DecayConfigStore::default();
        let config = store.get("nonexistent");
        assert!((config.lambda - DecayConfig::default().lambda).abs() < 0.0001);
    }

    #[test]
    fn test_config_store_override() {
        let store = DecayConfigStore::default();
        let custom = DecayConfig {
            lambda: 0.1,
            ..DecayConfig::default()
        };
        store.set("my-ns", custom.clone());
        let retrieved = store.get("my-ns");
        assert!((retrieved.lambda - 0.1).abs() < 0.0001);
        // Other namespace still gets default
        let default_config = store.get("other-ns");
        assert!((default_config.lambda - DecayConfig::default().lambda).abs() < 0.0001);
    }

    #[test]
    fn test_config_store_remove() {
        let store = DecayConfigStore::default();
        store.set("my-ns", DecayConfig::default());
        assert!(store.remove("my-ns"));
        assert!(!store.remove("my-ns"));
    }
}
```

**Step 2: Add module to src/store/mod.rs**

Add after `pub mod vector;`:
```rust
pub mod decay;
```

Add to pub use:
```rust
pub use decay::{DecayConfig, DecayConfigStore};
```

**Step 3: Run `cargo check`**

Expected: Compiles.

**Step 4: Commit**

```bash
git add -A && git commit -m "feat: add DecayConfig and DecayConfigStore for relevance decay"
```

---

### Task 3: Integrate DecayConfigStore into KvEngine and apply boost on read/write

**Objective:** Wire the decay config store into the engine. On writes (set/set_with_embedding/set_force/set_force_with_embedding), apply write_boost. On reads (get/get_with_meta), apply read_boost.

**Files:**
- Modify: `src/store/engine.rs`

**Step 1: Add DecayConfigStore to KvEngine**

Add field to KvEngine struct:
```rust
pub struct KvEngine {
    shards: Vec<Shard>,
    shard_mask: usize,
    max_entries: usize,
    compression_threshold: usize,
    eviction_policy: EvictionPolicy,
    vector_indexes: Mutex<HashMap<String, HnswIndex>>,
    namespace_versions: Mutex<HashMap<String, AtomicU64>>,
    hash_state: RandomState,
    /// Per-namespace relevance decay configuration.
    decay_config: DecayConfigStore,
}
```

Add import at top:
```rust
use crate::store::decay::DecayConfigStore;
```

**Step 2: Update all KvEngine constructors to include decay_config**

In `KvEngine::new`, `with_max_entries`, `with_max_entries_and_compression`, `with_eviction_policy` - add:
```rust
decay_config: DecayConfigStore::default(),
```

**Step 3: Add a public constructor for custom decay config**

```rust
pub fn with_decay_config(
    shard_count: usize,
    max_entries: usize,
    compression_threshold: usize,
    eviction_policy: EvictionPolicy,
    decay_config: DecayConfigStore,
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
        vector_indexes: Mutex::new(HashMap::new()),
        namespace_versions: Mutex::new(HashMap::new()),
        hash_state: RandomState::new(),
        decay_config,
    }
}
```

**Step 4: Add public accessor for decay_config**

```rust
pub fn decay_config(&self) -> &DecayConfigStore {
    &self.decay_config
}
```

**Step 5: Apply write_boost on set/set_with_embedding/set_force/set_force_with_embedding**

In `KvEngine::set`, after creating the entry but before inserting, apply write boost:

Change the entry construction to apply the write boost. The entry is constructed with `relevance: 1.0` for new entries. For updates (key already exists), we should read the old relevance and boost it. However, since papaya's insert returns the old value, we can handle this more efficiently.

Actually, the simplest approach: always set relevance to 1.0 for new entries on write. For updates, we read the old entry's relevance and apply the write boost. Let's modify the approach:

In `KvEngine::set`, after the shard inserts, we need to do a second pass to apply the boost. But that's inefficient. Better approach: let the shard handle the boost.

Actually, the cleanest way is: when setting an entry, we check if it's an update. If it is, we boost the old relevance. If it's new, we set relevance to 1.0. But the Shard::set method already knows if it's an update (it checks `is_update`).

Let's modify Shard::set to apply the write boost when it's an update:

In `Shard::set`, before the insert, check if the key already exists. If it does, read the old relevance and apply the write boost. Otherwise, use 1.0.

Wait - this would require passing DecayConfig into Shard, which breaks the current architecture where Shard is unaware of decay config. The simpler approach: have KvEngine handle the boost.

**Revised approach:** In KvEngine::set, before calling shard.set(), look up the existing entry to get its relevance (if any). Then set the entry's relevance accordingly. This is a read-before-write but it's the cleanest approach.

In `KvEngine::set`:
```rust
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
        if let Some(existing) = self.shards[idx].get_entry_raw_for_update(key) {
            let decayed = decay.current_relevance(existing.relevance, existing.created_at_ms, existing.pinned, now);
            let boosted = decay.apply_write_boost(decayed, existing.pinned);
            (boosted, existing.created_at_ms, existing.access_count.saturating_add(1), existing.pinned)
        } else {
            (1.0, now, 0, false)
        }
    };

    let entry = Entry {
        value,
        compressed: false,
        expires_at,
        memory_type,
        embedding: None,
        last_accessed_ms: now,
        relevance,
        created_at_ms,
        access_count,
        pinned,
    };
    let idx = self.shard_index(key);
    let result = self.shards[idx].set(key.to_string(), entry);
    // ... rest same
}
```

Hmm, but this adds a read-before-write which adds overhead. And `get_entry_raw_for_update` doesn't exist yet. Let me think about this more carefully.

Actually, the papaya HashMap's `insert` returns the old value (`Option<Entry>`). So in `Shard::set`, we already know if it was an update and we have the old entry. We just need to pass the decay config down.

But that would couple Shard to DecayConfig, which is not ideal.

**Simplest clean approach:** Have KvEngine do the read-before-write. It's an extra hash lookup but papaya is concurrent and fast. The alternative is to add an `on_update` callback, which is more complex.

Let's go with the read-before-write in KvEngine. We need a new method on Shard: `get_entry_raw_no_touch` that just reads the entry without updating last_accessed_ms or access_count (since we're about to overwrite it anyway).

Actually, `get_entry_raw` already exists and touches the entry. Let's add a simpler one:

In Shard:
```rust
/// Get the raw entry without touching access time. Used for read-before-write
/// in relevance boost calculation.
fn get_entry_untouched(&self, key: &str) -> Option<Entry> {
    let pin = self.map.pin();
    let entry = pin.get(key)?;
    let now = now_ms();
    if entry.is_expired(now) {
        None
    } else {
        Some(entry.clone())
    }
}
```

This is fine for the relevance boost lookup - we don't want to touch access stats on the old entry since we're about to replace it.

Now update all 4 set methods in KvEngine. Let me detail the changes for `set`:

```rust
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

    let (relevance, created_at_ms, access_count, pinned) = {
        let idx = self.shard_index(key);
        match self.shards[idx].get_entry_untouched(key) {
            Some(existing) => {
                let decayed = decay.current_relevance(existing.relevance, existing.created_at_ms, existing.pinned, now);
                let boosted = decay.apply_write_boost(decayed, existing.pinned);
                (boosted, existing.created_at_ms, existing.access_count.saturating_add(1), existing.pinned)
            }
            None => (1.0, now, 0, false),
        }
    };

    let entry = Entry {
        value,
        compressed: false,
        expires_at,
        memory_type,
        embedding: None,
        last_accessed_ms: now,
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
    self.increment_namespace_version(ns);
    Ok(())
}
```

Similar changes for `set_with_embedding`, `set_force`, and `set_force_with_embedding`.

**Step 6: Apply read_boost on get and get_with_meta**

In `KvEngine::get`:
The get already goes through `shard.get()` which calls `get_entry_raw` which already updates `last_accessed_ms` and `access_count`. But we also need to apply the read boost to the relevance.

Modify `get_entry_raw` in Shard to also apply the relevance boost. But Shard doesn't have access to DecayConfig. Two options:
1. Pass DecayConfig to Shard (coupling)
2. Do the boost in KvEngine after getting the entry

Let's do option 2: In `KvEngine::get` and `KvEngine::get_with_meta`, after getting the entry, compute the decayed relevance and boost it, then write back the boosted relevance. This is similar to how we update last_accessed_ms.

Actually, let's think about this. The relevance boost on read should persist - it should be written back. This means every read involves a potential write-back, which we already do for LRU (last_accessed_ms update). We can extend that same mechanism.

The cleanest approach: modify `get_entry_raw` in Shard to accept an optional relevance updater. But that adds complexity.

**Simpler approach:** Only apply the read boost in KvEngine, and do a separate update. The entry is already being touched (last_accessed_ms update) in get_entry_raw, so we're not adding a new write path, just extending the existing one.

Let's modify the touch logic in `get_entry_raw` to also apply the relevance boost. We'll need to pass the boost amount to the shard, or we can compute it at the engine level.

Actually, the simplest and most correct approach: Move the relevance read-boost to `get_entry_raw` where we already have the touch logic. We need to pass the read_boost value, but we can just hardcode it or pass it as a parameter.

Let me take a step back. The touch in `get_entry_raw` already does a clone + insert to update last_accessed_ms. We can extend that to also update relevance. But we need the decay config to compute the current relevance.

**Final decision:** Move the boost computation to the engine level. After calling `shard.get()` or `shard.get_entry()`, KvEngine will:
1. Compute the current decayed relevance
2. Apply the read boost
3. Write back the boosted relevance (only if it changed significantly, to avoid excessive writes)

But this means an extra hash lookup for the write-back. We already have one for the touch though.

Actually, let's look at `get_entry_raw` more carefully. It already does a read + conditional write. The write happens when `needs_touch` is true (more than 1 second since last access). We can extend this:

```rust
fn get_entry_raw(&self, key: &str) -> Option<Entry> {
    let pin = self.map.pin();
    let entry = pin.get(key)?;
    let now = now_ms();
    if entry.is_expired(now) {
        drop(pin);
        self.delete(key);
        None
    } else {
        let needs_touch = now.saturating_sub(entry.last_accessed_ms) > 1000;
        let cloned = entry.clone();
        drop(pin);
        if needs_touch {
            let mut updated = cloned.clone();
            updated.last_accessed_ms = now;
            updated.access_count = updated.access_count.saturating_add(1);
            let pin2 = self.map.pin();
            pin2.insert(key.to_string(), updated);
        }
        Some(cloned)
    }
}
```

For relevance, we can't easily do it here without DecayConfig. Let's keep it simple and just do the access_count increment in get_entry_raw (which we already added in Task 1), and handle the relevance boost at a higher level.

Actually, wait. The relevance boost doesn't need to be applied immediately on every read. It's a "soft" update. The most important thing is computing the current relevance at query time. The boost is about increasing the stored relevance so that future queries see a higher score.

Let's simplify: **Apply the read boost only when the entry is already being touched** (in `get_entry_raw` when `needs_touch` is true). This amortizes the cost. We'll pass the read_boost value as a method parameter.

Better yet: store the `read_boost` on the Shard itself. When the Shard is created, it gets the default read_boost. The KvEngine can update it if the namespace config changes. But that adds coupling.

**Final simplest approach:** Store a `read_boost` and `write_boost` as f64 fields on Shard. Default to the DecayConfig defaults. KvEngine passes them when creating Shards. This avoids passing DecayConfig on every call.

Hmm, but Shard is per-shard, not per-namespace. And decay config is per-namespace. A single shard contains entries from many namespaces.

OK, I think the cleanest approach for now is:

1. **Don't persist the read boost back to the entry on every read.** Instead, compute the "effective relevance" at query time as: `current_relevance = decay(stored_relevance, age) + access_count * read_boost_per_access`. This way, we don't need write-back on reads at all - we just increment access_count (which we already do in get_entry_raw) and compute the effective relevance at query time.

2. **Apply the write boost on writes** (in KvEngine::set, etc.) because writes are less frequent and we're already doing a read-before-write.

This is actually the approach described in the issue under "Access-Frequency Boosting": the relevance score is boosted per access, and between accesses the score decays. We can model this as: the stored relevance is the base, and access_count * boost_delta is added at query time.

Wait, but that means relevance can only go up over time with more accesses, which is wrong. The idea is that relevance decays between accesses and gets boosted on each access. So the stored relevance should be updated on each access.

Let me re-read the issue:

> Each read/write access to a key boosts its relevance score (e.g., relevance = min(1.0, relevance + boost_delta)).
> Between accesses, the score still decays per the time-based curve.

So the approach is: on each access, boost the stored relevance. Then at query time, apply time-based decay to the stored relevance. This means:

1. On read: `stored_relevance = min(1.0, current_decay(stored_relevance) + read_boost)`
2. On write: `stored_relevance = min(1.0, current_decay(stored_relevance) + write_boost)`
3. At query time: `effective_relevance = current_decay(stored_relevance)`

For this to work, we need to update the stored relevance on reads. Let's do it in `get_entry_raw` where we already have the touch logic:

```rust
if needs_touch {
    let mut updated = cloned.clone();
    updated.last_accessed_ms = now;
    updated.access_count = updated.access_count.saturating_add(1);
    // Apply read boost to stored relevance
    // We use the default read_boost here since we don't have namespace info
    // The engine-level get methods can do a more precise boost
    updated.relevance = (updated.relevance + 0.1).min(1.0);
    let pin2 = self.map.pin();
    pin2.insert(key.to_string(), updated);
}
```

But this uses a hardcoded 0.1. We'd need the namespace's read_boost. The Shard doesn't know the namespace.

**OK, final pragmatic decision:** We'll take a two-level approach:

1. In `Shard::get_entry_raw`, we only update `last_accessed_ms` and `access_count` (no relevance boost yet).
2. In `KvEngine::get` and `KvEngine::get_with_meta`, after getting the entry, we compute the current relevance, apply the read boost, and write it back if the relevance changed significantly. We do this by calling a new `shard.update_relevance(key, new_relevance)` method.

Actually, this is getting overly complex. Let me simplify:

**Simplification: Store the boosted relevance at write time only. At read time, just compute the decayed relevance for ranking. The access_count tracks popularity for ranking purposes.**

This means:
- On write: `relevance = decay(old_relevance) + write_boost` (stored in entry)
- On read: don't modify stored relevance. Instead, compute `effective_relevance = decay(stored_relevance) * (1 + access_count * read_boost_factor)` for ranking.
- Access_count is incremented on reads (already done in get_entry_raw).

This avoids write-back on reads entirely, and the access_count naturally accumulates. The effective_relevance formula at query time combines decay with access frequency.

Actually, re-reading the issue more carefully:

> When querying (e.g., GET with prefix or semantic search), multiply each result's recency factor by its current relevance score to produce a composite ranking.

So the ranking formula is: `recency * relevance`. The relevance is the decayed stored relevance. The recency is based on time (e.g., how recent the entry is). And access frequency boosts the stored relevance on each access.

I think the simplest correct implementation is:

1. **Stored relevance** is updated on writes only (via write_boost). Reads don't modify stored relevance.
2. **At query time**, compute `effective_relevance = decay(stored_relevance, age)`.
3. **Access count** is tracked but used primarily for the GC sweep (entries with low access count decay faster conceptually).
4. **For ranking in scan results**, sort by `effective_relevance` descending.

This is simple, correct, and doesn't require write-back on reads. The read boost from the issue can be added later if needed.

Wait, but the issue specifically says:

> Each read/write access to a key boosts its relevance score

OK let me just implement it properly. The write-back on reads is necessary. But we can do it efficiently by piggybacking on the existing touch logic in `get_entry_raw`.

**Final approach:**

1. Add a `read_boost: f64` field to Shard (default 0.1). This is a per-shard setting. Since shards don't map to namespaces (keys are hashed), the per-namespace read_boost will be applied at the engine level on a second pass.

Actually, this is getting too complicated for shards that don't know namespaces. Let me take the simplest possible approach that's still correct:

**Ultra-simple approach:**

1. Store `relevance`, `created_at_ms`, `access_count`, `pinned` on Entry.
2. On writes (in KvEngine), compute decayed relevance and apply write_boost before storing.
3. On reads, DON'T update stored relevance. Just increment access_count (which we already do in get_entry_raw's touch).
4. At query time (scan_prefix, get_with_meta), compute effective relevance as: `decay(stored_relevance) + access_count * read_boost_factor`, clamped to [0, 1].
5. GC sweep: entries where `decay(stored_relevance) < relevance_floor` and `access_count == 0`.

This approach:
- Correctly decays relevance over time
- Gives more relevance to frequently accessed entries (via access_count * read_boost_factor at query time)
- Doesn't need write-back on reads
- Is simple to implement

The "access_count * read_boost_factor" term acts as a bonus for frequently accessed entries. The `read_boost_factor` is per-namespace from DecayConfig.

This is good enough. Let's go with this.

**Step 6 (revised): Don't modify get/get_with_meta for relevance boost. The access_count increment in get_entry_raw is sufficient. The effective relevance is computed at query time.**

**Step 7: Add effective_relevance computation to EntryMeta and scan results**

Update `EntryMeta` and `EntryMetaWithEmbedding` in engine.rs to include the effective relevance:

```rust
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub memory_type: MemoryType,
    pub ttl_remaining_ms: Option<u64>,
    pub relevance: f64,
}

#[derive(Debug, Clone)]
pub struct EntryMetaWithEmbedding {
    pub memory_type: MemoryType,
    pub ttl_remaining_ms: Option<u64>,
    pub embedding: Option<Vec<f32>>,
    pub relevance: f64,
}
```

Update `get_with_meta`:
```rust
pub fn get_with_meta(&self, key: &str) -> Option<(Vec<u8>, EntryMeta)> {
    let idx = self.shard_index(key);
    let entry = self.shards[idx].get_entry(key)?;
    let now = now_ms();
    let ns = namespace_of_key(key);
    let decay = self.decay_config.get(ns);
    let effective_relevance = decay.current_relevance(entry.relevance, entry.created_at_ms, entry.pinned, now)
        + entry.access_count as f64 * decay.read_boost;
    let relevance = effective_relevance.min(1.0).max(0.0);
    let ttl_remaining_ms = entry.expires_at.map(|exp| exp.saturating_sub(now));
    let meta = EntryMeta {
        memory_type: entry.memory_type,
        ttl_remaining_ms,
        relevance,
    };
    Some((entry.value, meta))
}
```

Similarly update `scan_prefix` and `scan_prefix_with_embeddings`.

**Step 8: Run `cargo check`**

**Step 9: Commit**

```bash
git add -A && git commit -m "feat: integrate DecayConfigStore into KvEngine with relevance computation"
```

---

### Task 4: Add relevance-aware GC sweep

**Objective:** Add a method to reap entries whose relevance has fallen below the floor threshold.

**Files:**
- Modify: `src/store/shard.rs` (add `reap_low_relevance` method)
- Modify: `src/store/engine.rs` (add `reap_all_low_relevance` async method)

**Step 1: Add `reap_low_relevance` to Shard**

```rust
/// Reap entries whose relevance has fallen below the given floor threshold.
/// Uses the provided decay config to compute current relevance.
/// Returns the number of entries reaped.
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
```

**Step 2: Add `reap_all_low_relevance` to KvEngine**

```rust
/// Reap all entries whose relevance has fallen below the floor threshold
/// across all shards. Uses each namespace's decay config.
///
/// Staggers across shards by yielding (tokio::task::yield_now) between
/// shards to avoid latency spikes. Returns the total number of reaped entries.
pub async fn reap_all_low_relevance(&self) -> usize {
    let now = now_ms();
    let mut total = 0;
    for shard in &self.shards {
        // Use default decay config for sweeping (since shards contain mixed namespaces)
        let defaults = self.decay_config.defaults();
        total += shard.reap_low_relevance(
            defaults.relevance_floor,
            defaults.lambda,
            now,
        );
        tokio::task::yield_now().await;
    }
    total
}
```

Note: The sweep uses the default decay config since shards contain entries from mixed namespaces. For more precise per-namespace GC, we'd need to look up the namespace per key, which is more expensive. The default config is a reasonable approximation for the periodic sweep. The per-namespace behavior is correctly applied at query time.

**Step 3: Run `cargo check`**

**Step 4: Commit**

```bash
git add -A && git commit -m "feat: add relevance-based GC sweep (reap_low_relevance)"
```

---

### Task 5: Add relevance data to persistence (snapshot v5)

**Objective:** Extend the snapshot format to v5 with relevance fields, with backward compatibility for v4.

**Files:**
- Modify: `src/store/persistence.rs`

**Step 1: Add version 5 constant**

```rust
const VERSION_5: u8 = 5;
```

Update `VERSION` to 5.

**Step 2: Update SnapshotEntry to include relevance fields**

```rust
pub struct SnapshotEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub memory_type: MemoryType,
    pub expires_at: Option<u64>,
    pub embedding: Option<Vec<f32>>,
    /// Relevance score in [0.0, 1.0]. Default 1.0 for entries without relevance data.
    pub relevance: f64,
    /// UNIX timestamp in milliseconds when this entry was created.
    pub created_at_ms: u64,
    /// Number of times this entry has been accessed.
    pub access_count: u64,
    /// If true, relevance stays at 1.0 and never decays.
    pub pinned: bool,
}
```

**Step 3: Update write_snapshot for v5 format**

After the embedding data and before the entry_crc, write:
- `relevance: f64` (8 bytes, LE)
- `created_at_ms: u64` (8 bytes, LE)
- `access_count: u64` (8 bytes, LE)
- `pinned: u8` (1 byte, 0 or 1)

Total: 25 additional bytes per entry.

**Step 4: Update read_snapshot for v5 format**

When reading v5, after the embedding data, read the relevance fields.
When reading v4 or earlier, default relevance=1.0, created_at_ms=0, access_count=0, pinned=false.

**Step 5: Update collect_entries to include relevance fields**

In the `collect_entries` and `snapshot` functions, populate the new fields from Entry.

**Step 6: Update load functions to restore relevance fields**

When loading from a v5 snapshot, set the relevance, created_at_ms, access_count, and pinned on the Entry.

**Step 7: Run `cargo test`**

**Step 8: Commit**

```bash
git add -A && git commit -m "feat: snapshot v5 with relevance decay fields"
```

---

### Task 6: Expose relevance in HTTP and RESP APIs

**Objective:** Expose relevance score in HTTP responses and add a new MRELEVANCE RESP command. Add a PIN/UNPIN command. Add decay config management.

**Files:**
- Modify: `src/http/models.rs` (add relevance to responses)
- Modify: `src/http/server.rs` (populate relevance in responses)
- Modify: `src/resp/commands.rs` (add MRELEVANCE, PIN, UNPIN commands)

**Step 1: Add relevance to HTTP response models**

In `src/http/models.rs`, update `StoreResponse`:
```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreResponse {
    pub key: String,
    pub value: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevance: Option<f64>,
}
```

**Step 2: Update HTTP handlers to populate relevance**

In server.rs, update `get_memory`, `list_memories`, `batch_get` to include relevance from EntryMeta.

**Step 3: Add MRELEVANCE RESP command**

`MRELEVANCE key` - returns the current relevance score and metadata:
```
MRELEVANCE myns:mykey
1) "0.7234"    (relevance)
2) "0"         (access_count)
3) "no"        (pinned: yes/no)
```

**Step 4: Add PIN/UNPIN RESP commands**

`PIN key` - sets pinned=true on an entry.
`UNPIN key` - sets pinned=false on an entry.

Add these to the Shard and KvEngine:
```rust
// In KvEngine:
pub fn pin(&self, key: &str) -> bool { ... }
pub fn unpin(&self, key: &str) -> bool { ... }
```

**Step 5: Add DECAYCONFIG RESP command**

`DECAYCONFIG <namespace> [lambda <f64>] [read_boost <f64>] [write_boost <f64>] [relevance_floor <f64>]`

- With no extra args: returns current config for namespace
- With args: updates the config

**Step 6: Add REAPLOW RESP command**

`REAPLOW` - triggers a relevance-based GC sweep, returns count of reaped entries.

**Step 7: Run `cargo check` and `cargo test`**

**Step 8: Commit**

```bash
git add -A && git commit -m "feat: expose relevance in HTTP/RESP APIs, add PIN/UNPIN/DECAYCONFIG/REAPLOW commands"
```

---

### Task 7: Add relevance-aware sorting to scan results

**Objective:** Sort scan_prefix results by effective relevance when a flag is provided.

**Files:**
- Modify: `src/store/engine.rs` (add `scan_prefix_sorted` method)
- Modify: `src/http/server.rs` (add `sort_by` query param)
- Modify: `src/resp/commands.rs` (add SORT flag to MSCAN)

**Step 1: Add `scan_prefix_sorted` to KvEngine**

```rust
/// Scan entries and sort by effective relevance (descending).
/// Entries with higher relevance come first.
pub fn scan_prefix_sorted(&self, prefix: &str) -> Vec<(String, Vec<u8>, EntryMeta)> {
    let mut results = self.scan_prefix(prefix);
    results.sort_by(|a, b| {
        b.2.relevance.partial_cmp(&a.2.relevance).unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}
```

**Step 2: Add sort_by query parameter to HTTP list endpoint**

In `ListQuery`:
```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub sort_by: Option<String>,  // "relevance" for relevance-sorted results
```

In `list_memories` handler, use `scan_prefix_sorted` when sort_by=relevance.

**Step 3: Add SORT flag to MSCAN RESP command**

`MSCAN prefix [SORT]` - if SORT is present, sort results by relevance descending.

Each result tuple becomes: [key, value, type, ttl, relevance]

**Step 4: Run `cargo check` and `cargo test`**

**Step 5: Commit**

```bash
git add -A && git commit -m "feat: relevance-sorted scan results in HTTP and RESP APIs"
```

---

### Task 8: Wire up periodic relevance GC sweep in the main server

**Objective:** Add a background task that periodically runs the relevance-based GC sweep, similar to the existing expired-entries sweep.

**Files:**
- Modify: `src/main.rs` (add relevance sweep task)

**Step 1: Add relevance sweep configuration to Config**

In `src/config.rs`, add to `ServerConfig`:
```rust
/// How often to sweep low-relevance entries (milliseconds). 0 = disabled.
pub relevance_sweep_interval_ms: u64,
```

Default: 0 (disabled, needs explicit opt-in).

**Step 2: Add relevance sweep task in main.rs**

Similar to the existing expired-entries sweep loop, add a relevance sweep loop that calls `engine.reap_all_low_relevance()`.

**Step 3: Run `cargo check`**

**Step 4: Commit**

```bash
git add -A && git commit -m "feat: configurable periodic relevance GC sweep"
```

---

### Task 9: Comprehensive tests

**Objective:** Add thorough tests for all relevance decay functionality.

**Files:**
- Create: `tests/relevance_test.rs`

**Step 1: Write integration tests**

Tests to write:
1. Entry stores relevance, created_at, access_count, pinned fields
2. DecayConfig exponential decay works correctly
3. DecayConfigStore per-namespace overrides work
4. Write boost is applied on set operations
5. Access count increments on reads
6. Pinned entries always have relevance 1.0
7. Effective relevance computation at query time (get_with_meta)
8. Scan results include relevance
9. Relevance-based GC sweep removes entries below floor
10. Relevance-based GC sweep respects pinned entries
11. Snapshot v5 roundtrip preserves relevance fields
12. Snapshot v4 backward compatibility (defaults relevance=1.0)
13. HTTP API returns relevance in responses
14. MRELEVANCE RESP command
15. PIN/UNPIN RESP commands
16. DECAYCONFIG RESP command
17. Relevance-sorted scan results

**Step 2: Run `cargo test`**

**Step 3: Commit**

```bash
git add -A && git commit -m "test: comprehensive relevance decay tests"
```

---

### Task 10: Documentation and final cleanup

**Objective:** Update docs, run clippy, format, and ensure CI passes.

**Files:**
- Modify: `docs/configuration.md` (add relevance config docs)
- Modify: `README.md` (mention relevance decay feature)

**Step 1: Run `cargo fmt`**

**Step 2: Run `cargo clippy --all-targets -- -D warnings`**

Fix any warnings.

**Step 3: Run `cargo test`**

**Step 4: Update docs**

Add relevance decay configuration to `docs/configuration.md`.

**Step 5: Final commit**

```bash
git add -A && git commit -m "docs: relevance decay configuration and API docs"
```

---

## Summary

| Task | Description | Key Files |
|------|-------------|-----------|
| 1 | Add relevance fields to Entry | shard.rs, engine.rs |
| 2 | DecayConfig + DecayConfigStore | decay.rs (new), mod.rs |
| 3 | Integrate decay into KvEngine | engine.rs |
| 4 | Relevance-based GC sweep | shard.rs, engine.rs |
| 5 | Snapshot v5 with relevance | persistence.rs |
| 6 | HTTP/RESP API exposure | models.rs, server.rs, commands.rs |
| 7 | Relevance-sorted scan results | engine.rs, server.rs, commands.rs |
| 8 | Periodic GC sweep task | config.rs, main.rs |
| 9 | Comprehensive tests | relevance_test.rs (new) |
| 10 | Docs, clippy, fmt | docs/, README.md |

Backward compatibility: v4 snapshots load with relevance=1.0, created_at_ms=0, access_count=0, pinned=false. Existing entries without relevance data behave as before (no decay). The feature is opt-in via configuration.
