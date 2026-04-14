# Summarization Triggers Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add built-in summarization triggers that fire when user-defined conditions are met, allowing consumers to compress memories (e.g., summarize old episodic entries into semantic ones) instead of just deleting them.

**Architecture:** A new `src/store/trigger.rs` module provides `TriggerManager` - a registry of named triggers with conditions and async callback actions. The store checks conditions after writes and via a background sweep. Triggers are store-agnostic: they provide the mechanism, the consumer provides the action (which may or may not use LLM). Triggers are non-blocking via `tokio::spawn`.

**Tech Stack:** Rust, tokio, serde (JSON), existing Basalt patterns (Mutex<HashMap>, background tasks, axum routes, RESP commands).

---

### Task 1: Create trigger module with core types

**Objective:** Define the core trigger types in a new `src/store/trigger.rs` file.

**Files:**
- Create: `src/store/trigger.rs`
- Modify: `src/store/mod.rs`

**Step 1: Create trigger.rs with core types**

```rust
//! Summarization triggers for automatic memory compression.
//!
//! Triggers fire when defined conditions are met (entry count, age, pattern match)
//! and invoke a consumer-provided async callback. The store provides the trigger
//! mechanism; the consumer decides how to summarize.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// Entries that matched the condition (key, value as bytes, memory type as string).
    pub matching_entries: Vec<TriggerEntry>,
    /// Timestamp when the trigger fired (ms since epoch).
    pub fired_at_ms: u64,
}

/// A single entry included in a trigger context.
#[derive(Debug, Clone)]
pub struct TriggerEntry {
    pub key: String,
    pub value: Vec<u8>,
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

/// An async trigger action callback.
pub type TriggerActionFn =
    Box<dyn Fn(TriggerContext) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A registered trigger.
pub struct Trigger {
    /// Unique ID for this trigger.
    pub id: String,
    /// Condition that causes this trigger to fire.
    pub condition: TriggerCondition,
    /// Async callback invoked when the condition is met.
    pub action: TriggerActionFn,
    /// Whether this trigger is active.
    pub enabled: bool,
    /// Minimum time between fires (ms). Default: 60000 (1 minute).
    pub cooldown_ms: u64,
    /// Last time this trigger fired (ms).
    pub last_fired_ms: Option<u64>,
}

/// Registry of summarization triggers.
pub struct TriggerManager {
    triggers: Mutex<HashMap<String, Trigger>>,
}

impl TriggerManager {
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

    /// Get a trigger by ID.
    pub fn get(&self, id: &str) -> Option<TriggerInfo> {
        let triggers = self.triggers.lock().unwrap();
        triggers.get(id).map(|t| TriggerInfo {
            id: t.id.clone(),
            condition: t.condition.clone(),
            enabled: t.enabled,
            cooldown_ms: t.cooldown_ms,
            last_fired_ms: t.last_fired_ms,
        })
    }

    /// List all registered triggers (metadata only, no action).
    pub fn list(&self) -> Vec<TriggerInfo> {
        let triggers = self.triggers.lock().unwrap();
        triggers
            .values()
            .map(|t| TriggerInfo {
                id: t.id.clone(),
                condition: t.condition.clone(),
                enabled: t.enabled,
                cooldown_ms: t.cooldown_ms,
                last_fired_ms: t.last_fired_ms,
            })
            .collect()
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

    /// Update last_fired_ms for a trigger. Returns false if not found.
    pub fn mark_fired(&self, id: &str, now_ms: u64) -> bool {
        let mut triggers = self.triggers.lock().unwrap();
        if let Some(t) = triggers.get_mut(id) {
            t.last_fired_ms = Some(now_ms);
            true
        } else {
            false
        }
    }

    /// Check if a trigger is off cooldown (can fire now).
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

    /// Fire a trigger by ID if conditions are met and off cooldown.
    /// Returns the TriggerContext if fired, None otherwise.
    /// The caller is responsible for spawning the async action.
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
            namespace: match &t.condition {
                TriggerCondition::MinEntries { namespace, .. } => namespace.clone(),
                TriggerCondition::MaxAge { namespace, .. } => namespace.clone(),
                TriggerCondition::MatchCount { namespace, .. } => namespace.clone(),
            },
            condition: t.condition.clone(),
            matching_entries,
            fired_at_ms: now_ms,
        })
    }

    /// Take the action out of a trigger temporarily to avoid holding the lock
    /// while executing the callback. Returns (action, should_reinsert).
    /// This is needed because TriggerActionFn is not Clone.
    pub fn take_action(&self, id: &str) -> Option<TriggerActionFn> {
        let mut triggers = self.triggers.lock().unwrap();
        let t = triggers.get_mut(&id.to_string())?;
        // Swap out the action with a no-op, we'll put it back
        let noop: TriggerActionFn = Box::new(|_| Box::pin(async {}));
        let action = std::mem::replace(&mut t.action, noop);
        Some(action)
    }

    /// Put the action back after executing it.
    pub fn return_action(&self, id: &str, action: TriggerActionFn) {
        let mut triggers = self.triggers.lock().unwrap();
        if let Some(t) = triggers.get_mut(id) {
            t.action = action;
        }
    }
}

/// Serializable trigger metadata (without the action callback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub id: String,
    pub condition: TriggerCondition,
    pub enabled: bool,
    pub cooldown_ms: u64,
    pub last_fired_ms: Option<u64>,
}

impl Default for TriggerManager {
    fn default() -> Self {
        Self::new()
    }
}
```

**Step 2: Add trigger module to mod.rs**

In `src/store/mod.rs`, add:
```rust
pub mod trigger;
pub use trigger::{TriggerCondition, TriggerContext, TriggerEntry, TriggerInfo, TriggerManager};
```

**Step 3: Build and verify compilation**

Run: `cargo check 2>&1 | head -30`
Expected: May have warnings but no errors about trigger module.

**Step 4: Commit**

```bash
git add src/store/trigger.rs src/store/mod.rs
git commit -m "feat: add trigger module with core types (issue #44)"
```

---

### Task 2: Add trigger condition checking to KvEngine

**Objective:** Add methods to KvEngine that check trigger conditions and collect matching entries.

**Files:**
- Modify: `src/store/engine.rs`

**Step 1: Add TriggerManager to KvEngine**

Add `trigger_manager: Arc<TriggerManager>` field to `KvEngine`.

In `KvEngine::new()`, create it:
```rust
trigger_manager: Arc::new(TriggerManager::new()),
```

Add public accessor:
```rust
pub fn trigger_manager(&self) -> &Arc<TriggerManager> {
    &self.trigger_manager
}
```

**Step 2: Add check_trigger_conditions method**

Add a method to KvEngine that, given a namespace and trigger condition, checks if the condition is met and returns matching entries:

```rust
/// Check if a trigger condition is met for a namespace and return matching entries.
pub fn check_trigger_condition(
    &self,
    condition: &TriggerCondition,
    now_ms: u64,
) -> Option<Vec<TriggerEntry>> {
    match condition {
        TriggerCondition::MinEntries { namespace, threshold } => {
            let prefix = format!("{}:", namespace);
            let count = self.count_prefix(&prefix);
            if count >= *threshold {
                let entries = self.collect_trigger_entries(&prefix, None, now_ms);
                Some(entries)
            } else {
                None
            }
        }
        TriggerCondition::MaxAge { namespace, max_age_ms } => {
            let prefix = format!("{}:", namespace);
            let cutoff = now_ms.saturating_sub(*max_age_ms);
            let entries = self.collect_trigger_entries(&prefix, Some(cutoff), now_ms);
            if entries.is_empty() {
                None
            } else {
                Some(entries)
            }
        }
        TriggerCondition::MatchCount { namespace, key_prefix, threshold } => {
            let full_prefix = format!("{}:{}", namespace, key_prefix);
            let count = self.count_prefix(&full_prefix);
            if count >= *threshold {
                let entries = self.collect_trigger_entries(&full_prefix, None, now_ms);
                Some(entries)
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
    let config = self.decay_config();
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
            let namespace = key.split_once(':').map(|(ns, _)| ns.to_string()).unwrap_or_default();
            let dc = config.get(&namespace);
            let relevance = dc.current_relevance(
                entry.relevance,
                entry.created_at_ms,
                entry.pinned,
                now_ms,
            );
            TriggerEntry {
                key: key.clone(),
                value: entry.value.clone(),
                memory_type: format!("{:?}", entry.memory_type).to_lowercase(),
                relevance,
                created_at_ms: entry.created_at_ms,
                access_count: entry.access_count,
            }
        })
        .collect()
}
```

Note: `scan_prefix_entries` already exists on KvEngine and returns `Vec<(String, Entry)>`.

**Step 3: Verify compilation**

Run: `cargo check 2>&1 | head -30`
Expected: No errors.

**Step 4: Commit**

```bash
git add src/store/engine.rs
git commit -m "feat: add trigger condition checking to KvEngine (issue #44)"
```

---

### Task 3: Add background trigger sweep task

**Objective:** Add a background task that periodically checks all registered triggers and fires those whose conditions are met.

**Files:**
- Modify: `src/main.rs`

**Step 1: Add trigger sweep interval to config**

In `src/config.rs`, add to `ServerConfig`:
```rust
/// How often (ms) to check trigger conditions. 0 = disabled.
pub trigger_sweep_interval_ms: u64,
```

In the `Default` impl:
```rust
trigger_sweep_interval_ms: 30_000, // 30 seconds
```

In `ServerFile`:
```rust
trigger_sweep_interval_ms: Option<u64>,
```

In `Config::from_file()`, map the field.

**Step 2: Add trigger sweep background task in main.rs**

After the existing background tasks (snapshot loop, decay reap loop), add:

```rust
// Trigger sweep loop
{
    let engine = engine.clone();
    let trigger_mgr = engine.trigger_manager().clone();
    tokio::spawn(async move {
        if cfg.server.trigger_sweep_interval_ms == 0 {
            return; // disabled
        }
        let mut interval = tokio::time::interval(
            Duration::from_millis(cfg.server.trigger_sweep_interval_ms)
        );
        loop {
            interval.tick().await;
            let triggers = trigger_mgr.list();
            let now_ms = crate::time::now_ms();
            for info in triggers {
                if !info.enabled {
                    continue;
                }
                if let Some(entries) = engine.check_trigger_condition(&info.condition, now_ms) {
                    // Check cooldown and fire
                    if let Some(ctx) = trigger_mgr.try_fire(&info.id, entries, now_ms) {
                        // Take the action out to avoid holding the lock during execution
                        if let Some(action) = trigger_mgr.take_action(&info.id) {
                            let action_clone = // We need to handle this differently
                            // Since TriggerActionFn is not Clone, we use take/return pattern
                            let trigger_id = info.id.clone();
                            let trigger_mgr_clone = trigger_mgr.clone();
                            tokio::spawn(async move {
                                action(ctx).await;
                                // Action will be dropped; we need a different approach
                            });
                            // Problem: we can't return the action after it's been moved into a spawn
                        }
                    }
                }
            }
        }
    });
}
```

IMPORTANT: The take_action/return_action pattern won't work because the action is moved into a tokio::spawn. We need a different approach. Let's use Arc<dyn Fn> instead of Box<dyn Fn> so the action is cloneable:

**REVISED APPROACH for Task 1**: Change `TriggerActionFn` to use `Arc`:

```rust
pub type TriggerActionFn = Arc<dyn Fn(TriggerContext) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
```

This makes it Clone-able and avoids the take/return pattern. Remove `take_action` and `return_action` from TriggerManager.

Then in the background task:
```rust
if let Some(ctx) = trigger_mgr.try_fire(&info.id, entries, now_ms) {
    let triggers_lock = trigger_mgr.triggers.lock().unwrap();
    if let Some(t) = triggers_lock.get(&info.id) {
        let action = t.action.clone(); // Arc clone - cheap
        drop(triggers_lock); // Release lock before spawning
        tokio::spawn(async move {
            action(ctx).await;
        });
    }
}
```

**Step 3: Verify compilation**

Run: `cargo check 2>&1 | head -30`

**Step 4: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: add trigger sweep background task (issue #44)"
```

---

### Task 4: Add trigger management HTTP routes

**Objective:** Add HTTP API endpoints for registering, listing, enabling/disabling, and removing triggers.

**Files:**
- Modify: `src/http/models.rs` - Add request/response models
- Modify: `src/http/server.rs` - Add routes and handlers

**Step 1: Add HTTP models in models.rs**

```rust
// --- Trigger models ---

#[derive(Debug, Deserialize)]
pub struct RegisterTriggerRequest {
    pub id: String,
    pub condition: TriggerCondition,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

fn default_cooldown_ms() -> u64 {
    60_000
}

#[derive(Debug, Serialize)]
pub struct TriggerInfoResponse {
    pub id: String,
    pub condition: TriggerCondition,
    pub enabled: bool,
    pub cooldown_ms: u64,
    pub last_fired_ms: Option<u64>,
}

impl From<crate::store::trigger::TriggerInfo> for TriggerInfoResponse {
    fn from(info: crate::store::trigger::TriggerInfo) -> Self {
        TriggerInfoResponse {
            id: info.id,
            condition: info.condition,
            enabled: info.enabled,
            cooldown_ms: info.cooldown_ms,
            last_fired_ms: info.last_fired_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TriggerListResponse {
    pub triggers: Vec<TriggerInfoResponse>,
}

#[derive(Debug, Serialize)]
pub struct TriggerActionResponse {
    pub ok: bool,
    pub message: String,
}
```

NOTE: The HTTP register endpoint creates a "webhook-only" trigger (no in-process callback). The webhook URL is stored as part of the action configuration. For HTTP API, we add a webhook variant:

Actually, we need to reconsider. The issue says the trigger mechanism should support:
1. Callback-based (Rust API)
2. Webhook (HTTP POST)
3. Plugin

For the HTTP API, we only need webhook support. For the Rust API, we need the callback. Let's add a `WebhookAction` variant:

**REVISED**: Add a `TriggerActionConfig` enum that is serializable:

```rust
/// Configurable action for a trigger (serializable, used in HTTP API).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerActionConfig {
    /// POST matching entries to a URL. Response body is stored as new entries.
    Webhook {
        url: String,
        /// Optional headers to include (e.g., auth).
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Log the trigger event (for debugging/monitoring).
    Log,
}
```

And update `Trigger` to store both:
```rust
pub struct Trigger {
    pub id: String,
    pub condition: TriggerCondition,
    pub action: TriggerActionFn,        // In-process callback
    pub action_config: Option<TriggerActionConfig>,  // Serializable config (for HTTP/RESP)
    pub enabled: bool,
    pub cooldown_ms: u64,
    pub last_fired_ms: Option<u64>,
}
```

When registering via HTTP/RESP, `action` is a no-op and `action_config` is set. When the background sweep fires a trigger with a `Webhook` action_config, it POSTs the entries to the URL. When registering via Rust API, `action` is the real callback and `action_config` is None.

**Step 2: Add HTTP routes in server.rs**

Add to the protected router:
```rust
.route("/trigger", post(register_trigger))
.route("/trigger", get(list_triggers))
.route("/trigger/{id}", delete(delete_trigger))
.route("/trigger/{id}/enable", post(enable_trigger))
.route("/trigger/{id}/disable", post(disable_trigger))
.route("/trigger/{id}/fire", post(manual_fire_trigger))
```

Handler implementations:
- `register_trigger`: Parse `RegisterTriggerRequest`, create a Trigger with a no-op action and the webhook action_config if provided, call `trigger_manager.register()`.
- `list_triggers`: Return `TriggerListResponse` from `trigger_manager.list()`.
- `delete_trigger`: Call `trigger_manager.unregister()`.
- `enable_trigger` / `disable_trigger`: Call `trigger_manager.set_enabled()`.
- `manual_fire_trigger`: Check condition, collect entries, fire if met.

**Step 3: Implement webhook action execution**

In `src/store/trigger.rs`, add a function for executing webhook actions:

```rust
/// Execute a webhook trigger action.
pub async fn execute_webhook(config: &TriggerActionConfig, ctx: TriggerContext) -> Result<(), String> {
    match config {
        TriggerActionConfig::Webhook { url, headers } => {
            // POST the trigger context as JSON to the webhook URL
            let client = reqwest::Client::new();
            let body = serde_json::json!({
                "trigger_id": ctx.trigger_id,
                "namespace": ctx.namespace,
                "condition": ctx.condition,
                "matching_entries": ctx.matching_entries.iter().map(|e| serde_json::json!({
                    "key": e.key,
                    "value": String::from_utf8_lossy(&e.value),
                    "memory_type": e.memory_type,
                    "relevance": e.relevance,
                })).collect::<Vec<_>>(),
                "fired_at_ms": ctx.fired_at_ms,
            });
            let mut req = client.post(url).json(&body);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req.send().await
                .map_err(|e| format!("webhook request failed: {e}"))?;
            Ok(())
        }
        TriggerActionConfig::Log => {
            tracing::info!(
                "trigger fired: id={}, namespace={}, entries={}",
                ctx.trigger_id,
                ctx.namespace,
                ctx.matching_entries.len()
            );
            Ok(())
        }
    }
}
```

NOTE: This adds `reqwest` as a dependency. Since the LLM module likely already uses it, check Cargo.toml.

**Step 4: Update background sweep to handle webhook actions**

In the trigger sweep in main.rs, after getting the context, check if the trigger has an `action_config`:

```rust
if let Some(ctx) = trigger_mgr.try_fire(&info.id, entries, now_ms) {
    let action_config = {
        let triggers = trigger_mgr.triggers.lock().unwrap();
        triggers.get(&info.id).and_then(|t| t.action_config.clone())
    };
    if let Some(config) = action_config {
        tokio::spawn(async move {
            if let Err(e) = execute_webhook(&config, ctx).await {
                tracing::warn!("trigger webhook failed: id={}, error={}", info.id, e);
            }
        });
    } else {
        // In-process callback
        let action = {
            let triggers = trigger_mgr.triggers.lock().unwrap();
            triggers.get(&info.id).map(|t| t.action.clone())
        };
        if let Some(action) = action {
            tokio::spawn(async move {
                action(ctx).await;
            });
        }
    }
}
```

**Step 5: Verify compilation**

Run: `cargo check 2>&1 | head -30`

**Step 6: Commit**

```bash
git add src/store/trigger.rs src/http/models.rs src/http/server.rs src/main.rs
git commit -m "feat: add trigger HTTP routes and webhook action (issue #44)"
```

---

### Task 5: Add trigger RESP commands

**Objective:** Add RESP commands for trigger management: TRIGGER ADD, TRIGGER DEL, TRIGGER LIST, TRIGGER FIRE, TRIGGER ENABLE, TRIGGER DISABLE.

**Files:**
- Modify: `src/resp/commands.rs`

**Step 1: Add TRIGGER command dispatch**

In `CommandHandler::handle()`, add:
```rust
"TRIGGER" => self.handle_trigger(cmd),
```

**Step 2: Implement handle_trigger**

```rust
fn handle_trigger(&self, cmd: &Command) -> RespValue {
    let sub = match cmd.args.get(1) {
        Some(s) => s.to_uppercase(),
        None => return RespValue::Error("ERR TRIGGER requires a subcommand: ADD|DEL|LIST|FIRE|ENABLE|DISABLE".into()),
    };
    match sub.as_str() {
        "ADD" => self.handle_trigger_add(cmd),
        "DEL" => self.handle_trigger_del(cmd),
        "LIST" => self.handle_trigger_list(cmd),
        "FIRE" => self.handle_trigger_fire(cmd),
        "ENABLE" => self.handle_trigger_enable(cmd),
        "DISABLE" => self.handle_trigger_disable(cmd),
        _ => RespValue::Error(format!("ERR unknown TRIGGER subcommand: {sub}")),
    }
}
```

Subcommands:
- `TRIGGER ADD <id> <condition_json> [cooldown_ms]` - Register a trigger with JSON condition
- `TRIGGER DEL <id>` - Remove a trigger
- `TRIGGER LIST` - List all triggers
- `TRIGGER FIRE <id>` - Manually fire a trigger
- `TRIGGER ENABLE <id>` - Enable a trigger
- `TRIGGER DISABLE <id>` - Disable a trigger

**Step 3: Verify compilation**

Run: `cargo check 2>&1 | head -30`

**Step 4: Commit**

```bash
git add src/resp/commands.rs
git commit -m "feat: add TRIGGER RESP commands (issue #44)"
```

---

### Task 6: Add tests for trigger module

**Objective:** Comprehensive tests for the trigger system.

**Files:**
- Modify: `src/store/trigger.rs` (add #[cfg(test)] module)

**Step 1: Add unit tests**

Tests to write:
1. `test_register_and_list` - Register triggers, verify they appear in list
2. `test_register_duplicate_fails` - Registering same ID twice returns Err
3. `test_unregister` - Remove a trigger
4. `test_enable_disable` - Toggle trigger enabled state
5. `test_cooldown_prevents_rapid_fire` - Fire once, verify can_fire returns false during cooldown
6. `test_cooldown_allows_after_wait` - Fire, wait past cooldown, verify can_fire returns true
7. `test_try_fire_returns_context` - Fire a trigger, verify context is populated correctly
8. `test_try_fire_disabled` - Disabled trigger returns None from try_fire
9. `test_condition_serialization` - Verify TriggerCondition serializes/deserializes from JSON
10. `test_webhook_action_config` - Verify TriggerActionConfig serialization

**Step 2: Run tests**

Run: `cargo test --lib trigger 2>&1 | tail -20`
Expected: All tests pass.

**Step 3: Commit**

```bash
git add src/store/trigger.rs
git commit -m "test: add trigger module unit tests (issue #44)"
```

---

### Task 7: Add integration tests

**Objective:** Integration tests for trigger HTTP routes and RESP commands.

**Files:**
- Create: `tests/trigger_test.rs`

**Step 1: Write HTTP integration tests**

Tests:
1. Register a MinEntries trigger via POST /trigger
2. List triggers via GET /trigger
3. Delete a trigger via DELETE /trigger/{id}
4. Enable/disable a trigger
5. Manual fire trigger via POST /trigger/{id}/fire

**Step 2: Write RESP integration tests**

Tests:
1. TRIGGER ADD with MinEntries condition
2. TRIGGER LIST
3. TRIGGER DEL
4. TRIGGER FIRE
5. TRIGGER ENABLE / TRIGGER DISABLE

**Step 3: Run tests**

Run: `cargo test --test trigger_test 2>&1 | tail -20`

**Step 4: Commit**

```bash
git add tests/trigger_test.rs
git commit -m "test: add trigger integration tests (issue #44)"
```

---

### Task 8: Add trigger config and documentation

**Objective:** Add TOML config support and documentation.

**Files:**
- Modify: `src/config.rs` - Add TriggerConfig section
- Modify: `basalt.example.toml` - Add [trigger] section
- Create: `docs/triggers.md` - Trigger documentation
- Modify: `README.md` - Add triggers to features list and docs table
- Modify: `docs/api-reference.md` - Add trigger HTTP/RESP API docs

**Step 1: Add TriggerConfig to config.rs**

```rust
#[derive(Debug, Clone, Default)]
pub struct TriggerConfig {
    /// Enable/disable trigger subsystem. Default: true.
    pub enabled: bool,
    /// How often (ms) to sweep trigger conditions. Default: 30000.
    pub sweep_interval_ms: u64,
    /// Default cooldown (ms) between trigger fires. Default: 60000.
    pub default_cooldown_ms: u64,
}
```

**Step 2: Add [trigger] section to basalt.example.toml**

```toml
[trigger]
enabled = true
sweep_interval_ms = 30000
default_cooldown_ms = 60000
```

**Step 3: Write docs/triggers.md**

Document:
- What summarization triggers are
- Trigger conditions (MinEntries, MaxAge, MatchCount)
- Action types (webhook, log, in-process callback)
- HTTP API reference
- RESP command reference
- Cooldown behavior
- Integration with relevance decay

**Step 4: Update README.md features list and docs table**

Add to features:
```
- **Summarization triggers** - Automatic memory compression when conditions are met (entry count, age, pattern match), with webhook and callback actions
```

Add to docs table:
```
| Summarization triggers | [docs/triggers.md](docs/triggers.md) |
```

**Step 5: Update docs/api-reference.md**

Add sections for:
- POST /trigger (register)
- GET /trigger (list)
- DELETE /trigger/{id}
- POST /trigger/{id}/enable
- POST /trigger/{id}/disable
- POST /trigger/{id}/fire (manual fire)
- TRIGGER ADD/DEL/LIST/FIRE/ENABLE/DOWN RESP commands

**Step 6: Commit**

```bash
git add src/config.rs basalt.example.toml docs/triggers.md README.md docs/api-reference.md
git commit -m "docs: add trigger configuration and documentation (issue #44)"
```

---

### Task 9: Final verification and cleanup

**Objective:** Full build, test, fmt, clippy pass.

**Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -30`

**Step 2: Run cargo fmt**

Run: `cargo fmt`

**Step 3: Run cargo clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | head -30`
Fix any warnings.

**Step 4: Final commit and push**

```bash
git add -A
git commit -m "feat: complete summarization triggers implementation (issue #44)"
git push
```
