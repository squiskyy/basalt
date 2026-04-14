//! Summarization triggers for automatic memory compression.
//!
//! Triggers fire when defined conditions are met (entry count, age, pattern match)
//! and invoke a consumer-provided async callback or webhook. The store provides the
//! trigger mechanism; the consumer decides how to summarize.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A fired trigger context, passed to the callback action.
#[derive(Debug, Clone)]
pub struct TriggerContext {
    /// ID of the trigger that fired.
    pub trigger_id: String,
    /// Namespace the trigger is scoped to.
    pub namespace: String,
    /// Which condition was met.
    pub condition: TriggerCondition,
    /// Entries that matched the condition.
    pub matching_entries: Vec<TriggerEntry>,
    /// Timestamp when the trigger fired (ms since epoch).
    pub fired_at_ms: u64,
}

/// A single entry included in a trigger context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEntry {
    pub key: String,
    /// Value as a UTF-8 string (lossy conversion from bytes).
    pub value: String,
    pub memory_type: String,
    pub relevance: f64,
    pub created_at_ms: u64,
    pub access_count: u64,
}

/// Condition that causes a trigger to fire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerCondition {
    /// Fire when namespace entry count >= threshold.
    MinEntries {
        namespace: String,
        threshold: usize,
    },
    /// Fire when oldest entry in namespace exceeds max_age_ms.
    MaxAge {
        namespace: String,
        max_age_ms: u64,
    },
    /// Fire when entries matching a key prefix reach threshold.
    MatchCount {
        namespace: String,
        key_prefix: String,
        threshold: usize,
    },
}

impl TriggerCondition {
    /// Return the namespace this condition is scoped to.
    pub fn namespace(&self) -> &str {
        match self {
            TriggerCondition::MinEntries { namespace, .. } => namespace,
            TriggerCondition::MaxAge { namespace, .. } => namespace,
            TriggerCondition::MatchCount { namespace, .. } => namespace,
        }
    }
}

/// Configurable action for a trigger (serializable, used in HTTP/RESP API).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerActionConfig {
    /// POST matching entries to a URL when the trigger fires.
    Webhook {
        url: String,
        /// Optional headers (e.g., auth).
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Log the trigger event (for debugging/monitoring).
    Log,
}

/// An async trigger action callback. Uses Arc for cheap cloning into tokio::spawn.
pub type TriggerActionFn =
    Arc<dyn Fn(TriggerContext) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

use std::sync::Arc;

/// A registered trigger.
pub struct Trigger {
    /// Unique ID for this trigger.
    pub id: String,
    /// Condition that causes this trigger to fire.
    pub condition: TriggerCondition,
    /// In-process async callback (used when registering via Rust API).
    pub action: TriggerActionFn,
    /// Serializable action config (used when registering via HTTP/RESP).
    /// When set, the background sweep uses this instead of the action callback.
    pub action_config: Option<TriggerActionConfig>,
    /// Whether this trigger is active.
    pub enabled: bool,
    /// Minimum time between fires (ms). Default: 60000 (1 minute).
    pub cooldown_ms: u64,
    /// Last time this trigger fired (ms).
    pub last_fired_ms: Option<u64>,
}

/// Serializable trigger metadata (without the action callback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub id: String,
    pub condition: TriggerCondition,
    pub action_config: Option<TriggerActionConfig>,
    pub enabled: bool,
    pub cooldown_ms: u64,
    pub last_fired_ms: Option<u64>,
}

impl TriggerInfo {
    /// Create a TriggerInfo from a Trigger (strips the non-serializable action).
    pub fn from_trigger(trigger: &Trigger) -> Self {
        TriggerInfo {
            id: trigger.id.clone(),
            condition: trigger.condition.clone(),
            action_config: trigger.action_config.clone(),
            enabled: trigger.enabled,
            cooldown_ms: trigger.cooldown_ms,
            last_fired_ms: trigger.last_fired_ms,
        }
    }
}

/// Registry of summarization triggers.
pub struct TriggerManager {
    triggers: Mutex<HashMap<String, Trigger>>,
}

impl TriggerManager {
    /// Create a new empty trigger manager.
    pub fn new() -> Self {
        TriggerManager {
            triggers: Mutex::new(HashMap::new()),
        }
    }

    /// Register a new trigger. Returns Err if ID already exists.
    pub fn register(&self, trigger: Trigger) -> Result<(), String> {
        let mut triggers = self.triggers.lock().unwrap();
        if triggers.contains_key(&trigger.id) {
            return Err(format!("trigger '{}' already exists", trigger.id));
        }
        triggers.insert(trigger.id.clone(), trigger);
        Ok(())
    }

    /// Unregister a trigger by ID. Returns true if it existed.
    pub fn unregister(&self, id: &str) -> bool {
        self.triggers.lock().unwrap().remove(id).is_some()
    }

    /// Get trigger metadata by ID.
    pub fn get(&self, id: &str) -> Option<TriggerInfo> {
        let triggers = self.triggers.lock().unwrap();
        triggers.get(id).map(TriggerInfo::from_trigger)
    }

    /// List all registered triggers (metadata only, no action callback).
    pub fn list(&self) -> Vec<TriggerInfo> {
        let triggers = self.triggers.lock().unwrap();
        triggers.values().map(TriggerInfo::from_trigger).collect()
    }

    /// Enable or disable a trigger. Returns false if not found.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut triggers = self.triggers.lock().unwrap();
        if let Some(t) = triggers.get_mut(id) {
            t.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Check if a trigger is off cooldown and enabled (can fire now).
    pub fn can_fire(&self, id: &str, now_ms: u64) -> bool {
        let triggers = self.triggers.lock().unwrap();
        if let Some(t) = triggers.get(id) {
            if !t.enabled {
                return false;
            }
            if let Some(last) = t.last_fired_ms {
                now_ms >= last + t.cooldown_ms
            } else {
                true
            }
        } else {
            false
        }
    }

    /// Fire a trigger by ID if enabled and off cooldown.
    /// Returns the TriggerContext if fired, None otherwise.
    /// Also marks the trigger as fired (updates last_fired_ms).
    pub fn try_fire(
        &self,
        id: &str,
        matching_entries: Vec<TriggerEntry>,
        now_ms: u64,
    ) -> Option<TriggerContext> {
        let mut triggers = self.triggers.lock().unwrap();
        let t = triggers.get_mut(id)?;
        if !t.enabled {
            return None;
        }
        if let Some(last) = t.last_fired_ms {
            if now_ms < last + t.cooldown_ms {
                return None;
            }
        }
        t.last_fired_ms = Some(now_ms);
        Some(TriggerContext {
            trigger_id: t.id.clone(),
            namespace: t.condition.namespace().to_string(),
            condition: t.condition.clone(),
            matching_entries,
            fired_at_ms: now_ms,
        })
    }

    /// Get the action callback for a trigger (Arc clone, cheap).
    pub fn get_action(&self, id: &str) -> Option<TriggerActionFn> {
        let triggers = self.triggers.lock().unwrap();
        triggers.get(id).map(|t| t.action.clone())
    }

    /// Get the action config for a trigger.
    pub fn get_action_config(&self, id: &str) -> Option<TriggerActionConfig> {
        let triggers = self.triggers.lock().unwrap();
        triggers.get(id).and_then(|t| t.action_config.clone())
    }

    /// Manually fire a trigger, ignoring cooldown (for the manual fire API).
    /// Still respects the enabled flag.
    pub fn force_fire(
        &self,
        id: &str,
        matching_entries: Vec<TriggerEntry>,
        now_ms: u64,
    ) -> Option<TriggerContext> {
        let mut triggers = self.triggers.lock().unwrap();
        let t = triggers.get_mut(id)?;
        if !t.enabled {
            return None;
        }
        t.last_fired_ms = Some(now_ms);
        Some(TriggerContext {
            trigger_id: t.id.clone(),
            namespace: t.condition.namespace().to_string(),
            condition: t.condition.clone(),
            matching_entries,
            fired_at_ms: now_ms,
        })
    }
}

impl Default for TriggerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Execute a webhook trigger action.
/// POSTs the trigger context as JSON to the configured URL.
pub async fn execute_webhook(
    config: &TriggerActionConfig,
    ctx: TriggerContext,
) -> Result<(), String> {
    match config {
        TriggerActionConfig::Webhook { url, headers } => {
            let client = reqwest::Client::new();
            let body = serde_json::json!({
                "trigger_id": ctx.trigger_id,
                "namespace": ctx.namespace,
                "condition": ctx.condition,
                "matching_entries": ctx.matching_entries,
                "fired_at_ms": ctx.fired_at_ms,
            });
            let mut req = client.post(url).json(&body);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req.send()
                .await
                .map_err(|e| format!("webhook request failed: {e}"))?;
            Ok(())
        }
        TriggerActionConfig::Log => {
            tracing::info!(
                trigger_id = ctx.trigger_id,
                namespace = ctx.namespace,
                entries = ctx.matching_entries.len(),
                "trigger fired"
            );
            Ok(())
        }
    }
}

/// Create a no-op trigger action (for HTTP/RESP registered triggers that use action_config).
fn noop_action() -> TriggerActionFn {
    Arc::new(|_ctx| Box::pin(async {}))
}

/// Create a new trigger with a no-op in-process action (for HTTP/RESP registration).
pub fn trigger_from_config(
    id: String,
    condition: TriggerCondition,
    action_config: Option<TriggerActionConfig>,
    cooldown_ms: u64,
) -> Trigger {
    Trigger {
        id,
        condition,
        action: noop_action(),
        action_config,
        enabled: true,
        cooldown_ms,
        last_fired_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_list() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        assert!(mgr.register(trigger).is_ok());
        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "t1");
    }

    #[test]
    fn test_register_duplicate_fails() {
        let mgr = TriggerManager::new();
        let t1 = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        let t2 = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MaxAge {
                namespace: "ns1".to_string(),
                max_age_ms: 86_400_000,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        assert!(mgr.register(t1).is_ok());
        assert!(mgr.register(t2).is_err());
    }

    #[test]
    fn test_unregister() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();
        assert!(mgr.unregister("t1"));
        assert!(!mgr.unregister("t1"));
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn test_enable_disable() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();
        assert!(mgr.set_enabled("t1", false));
        let info = mgr.get("t1").unwrap();
        assert!(!info.enabled);
        assert!(!mgr.can_fire("t1", 0));
    }

    #[test]
    fn test_cooldown_prevents_rapid_fire() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();

        let entries = vec![];
        // First fire succeeds
        let ctx = mgr.try_fire("t1", entries.clone(), 1000);
        assert!(ctx.is_some());

        // Second fire within cooldown fails
        let ctx = mgr.try_fire("t1", entries, 30_000);
        assert!(ctx.is_none());

        // can_fire also reflects cooldown
        assert!(!mgr.can_fire("t1", 30_000));
    }

    #[test]
    fn test_cooldown_allows_after_wait() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();

        let entries = vec![];
        let _ = mgr.try_fire("t1", entries.clone(), 1000);

        // After cooldown period, can fire again
        assert!(mgr.can_fire("t1", 62_000));
        let ctx = mgr.try_fire("t1", entries, 62_000);
        assert!(ctx.is_some());
    }

    #[test]
    fn test_try_fire_disabled() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: false,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();

        let ctx = mgr.try_fire("t1", vec![], 0);
        assert!(ctx.is_none());
    }

    #[test]
    fn test_try_fire_returns_context() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MaxAge {
                namespace: "ns2".to_string(),
                max_age_ms: 86_400_000,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 0,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();

        let entries = vec![TriggerEntry {
            key: "ns2:old".to_string(),
            value: "old data".to_string(),
            memory_type: "episodic".to_string(),
            relevance: 0.05,
            created_at_ms: 100,
            access_count: 0,
        }];

        let ctx = mgr.try_fire("t1", entries.clone(), 5000).unwrap();
        assert_eq!(ctx.trigger_id, "t1");
        assert_eq!(ctx.namespace, "ns2");
        assert_eq!(ctx.matching_entries.len(), 1);
        assert_eq!(ctx.fired_at_ms, 5000);
    }

    #[test]
    fn test_condition_serialization() {
        let cond = TriggerCondition::MinEntries {
            namespace: "agent-1".to_string(),
            threshold: 50,
        };
        let json = serde_json::to_string(&cond).unwrap();
        let parsed: TriggerCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(cond, parsed);

        let cond2 = TriggerCondition::MaxAge {
            namespace: "agent-2".to_string(),
            max_age_ms: 3600000,
        };
        let json2 = serde_json::to_string(&cond2).unwrap();
        let parsed2: TriggerCondition = serde_json::from_str(&json2).unwrap();
        assert_eq!(cond2, parsed2);

        let cond3 = TriggerCondition::MatchCount {
            namespace: "agent-3".to_string(),
            key_prefix: "obs:".to_string(),
            threshold: 10,
        };
        let json3 = serde_json::to_string(&cond3).unwrap();
        let parsed3: TriggerCondition = serde_json::from_str(&json3).unwrap();
        assert_eq!(cond3, parsed3);
    }

    #[test]
    fn test_webhook_action_config_serialization() {
        let config = TriggerActionConfig::Webhook {
            url: "https://example.com/hook".to_string(),
            headers: {
                let mut h = HashMap::new();
                h.insert("Authorization".to_string(), "Bearer token".to_string());
                h
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TriggerActionConfig = serde_json::from_str(&json).unwrap();
        match parsed {
            TriggerActionConfig::Webhook { url, headers } => {
                assert_eq!(url, "https://example.com/hook");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer token");
            }
            _ => panic!("expected Webhook variant"),
        }

        let log_config = TriggerActionConfig::Log;
        let json = serde_json::to_string(&log_config).unwrap();
        let parsed: TriggerActionConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TriggerActionConfig::Log));
    }

    #[test]
    fn test_trigger_from_config() {
        let trigger = trigger_from_config(
            "t1".to_string(),
            TriggerCondition::MatchCount {
                namespace: "ns1".to_string(),
                key_prefix: "obs:".to_string(),
                threshold: 5,
            },
            Some(TriggerActionConfig::Log),
            30_000,
        );
        assert_eq!(trigger.id, "t1");
        assert!(trigger.enabled);
        assert_eq!(trigger.cooldown_ms, 30_000);
        assert!(trigger.action_config.is_some());
    }

    #[test]
    fn test_force_fire_ignores_cooldown() {
        let mgr = TriggerManager::new();
        let trigger = Trigger {
            id: "t1".to_string(),
            condition: TriggerCondition::MinEntries {
                namespace: "ns1".to_string(),
                threshold: 100,
            },
            action: noop_action(),
            action_config: None,
            enabled: true,
            cooldown_ms: 60_000,
            last_fired_ms: None,
        };
        mgr.register(trigger).unwrap();

        let _ = mgr.try_fire("t1", vec![], 1000);
        // Normal fire blocked by cooldown
        assert!(mgr.try_fire("t1", vec![], 2000).is_none());
        // Force fire ignores cooldown
        let ctx = mgr.force_fire("t1", vec![], 2000);
        assert!(ctx.is_some());
    }
}
