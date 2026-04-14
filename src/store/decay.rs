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
    /// Default: ln(2) / 24.0 = ~0.0289 (24-hour halflife).
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
    /// Compute the current relevance of an entry given its stored relevance,
    /// creation time, pinned status, and current time.
    /// Uses exponential decay: relevance = stored_relevance * e^(-lambda * age_hours).
    /// For pinned entries, returns 1.0.
    pub fn current_relevance(
        &self,
        stored_relevance: f64,
        created_at_ms: u64,
        pinned: bool,
        now_ms: u64,
    ) -> f64 {
        if pinned {
            return 1.0;
        }
        if now_ms <= created_at_ms {
            return stored_relevance.clamp(0.0, 1.0);
        }
        let age_hours = (now_ms - created_at_ms) as f64 / 3_600_000.0;
        let decay = (-self.lambda * age_hours).exp();
        (stored_relevance * decay).clamp(0.0, 1.0)
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
        overrides
            .get(namespace)
            .cloned()
            .unwrap_or_else(|| self.defaults.clone())
    }

    /// Set a custom decay config for a namespace.
    pub fn set(&self, namespace: &str, config: DecayConfig) {
        self.overrides
            .lock()
            .unwrap()
            .insert(namespace.to_string(), config);
    }

    /// Remove a custom decay config for a namespace, reverting to default.
    pub fn remove(&self, namespace: &str) -> bool {
        self.overrides.lock().unwrap().remove(namespace).is_some()
    }

    /// List all namespaces with custom decay configs.
    pub fn list_overrides(&self) -> Vec<(String, DecayConfig)> {
        let overrides = self.overrides.lock().unwrap();
        overrides
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        let now: u64 = 1_000_000_000_000; // large timestamp
        let created = now - 86_400_000; // 24 hours ago
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!(
            (relevance - 0.5).abs() < 0.01,
            "24h halflife: expected ~0.5, got {relevance}"
        );
    }

    #[test]
    fn test_pinned_always_1() {
        let config = DecayConfig::default();
        let now: u64 = 1_000_000_000_000;
        let created = now - 86_400_000; // 24h ago
        let relevance = config.current_relevance(1.0, created, true, now);
        assert_eq!(relevance, 1.0, "pinned entries should have relevance 1.0");
    }

    #[test]
    fn test_decay_over_time() {
        let config = DecayConfig::default();
        let now = 1_000_000_000_000; // large enough timestamp in ms
        // 1 hour ago: should still be quite high
        let created = now - 3_600_000;
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!(
            relevance > 0.95,
            "1h old entry should have relevance > 0.95, got {relevance}"
        );

        // 72 hours ago: should be quite low
        let created = now - 72 * 3_600_000;
        let relevance = config.current_relevance(1.0, created, false, now);
        assert!(
            relevance < 0.15,
            "72h old entry should have relevance < 0.15, got {relevance}"
        );
    }

    #[test]
    fn test_read_boost() {
        let config = DecayConfig::default();
        let boosted = config.apply_read_boost(0.5, false);
        assert!(
            (boosted - 0.6).abs() < 0.001,
            "read boost: expected 0.6, got {boosted}"
        );

        // Should clamp to 1.0
        let boosted = config.apply_read_boost(0.95, false);
        assert_eq!(boosted, 1.0, "read boost should clamp to 1.0");
    }

    #[test]
    fn test_write_boost() {
        let config = DecayConfig::default();
        let boosted = config.apply_write_boost(0.5, false);
        assert!(
            (boosted - 0.65).abs() < 0.001,
            "write boost: expected 0.65, got {boosted}"
        );

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
