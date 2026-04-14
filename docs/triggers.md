# Summarization Triggers

Summarization triggers automatically fire when defined conditions are met, allowing you to compress or process memories before they expire. This is useful for summarizing old episodic memories into semantic entries, triggering webhooks when namespaces grow too large, or logging trigger events for monitoring.

## Overview

The trigger system is **store-agnostic**: Basalt provides the trigger mechanism (condition detection, dispatch, bookkeeping), but does not bundle any LLM or summarization logic. The consumer decides what happens when a trigger fires - it could call an LLM, POST to a webhook, log the event, or anything else.

Triggers are **non-blocking**: they fire asynchronously via `tokio::spawn` and never block normal read/write operations.

## Trigger Conditions

| Condition | Description | Example |
|-----------|-------------|---------|
| `min_entries` | Fire when namespace entry count >= threshold | Compress when an agent has >500 episodic entries |
| `max_age` | Fire when any entry in namespace exceeds max_age_ms | Summarize entries older than 24 hours |
| `match_count` | Fire when entries matching a key prefix reach threshold | Process when >50 "obs:" entries accumulate |

## Trigger Actions

| Action | Description |
|--------|-------------|
| `webhook` | POST trigger context (matching entries, condition info) to a URL |
| `log` | Log the trigger event via `tracing::info` (for debugging) |

You can also register in-process async callbacks via the Rust API (useful when embedding Basalt as a library).

## Configuration

### TOML Config

```toml
[server]
trigger_sweep_interval_ms = 30000  # Check triggers every 30s (0 = disabled)
```

### Default Behavior

- Trigger sweep runs every 30 seconds by default
- Default cooldown between fires: 60 seconds (1 minute)
- Set `trigger_sweep_interval_ms = 0` to disable automatic trigger checking

## HTTP API

### Register a Trigger

```
POST /trigger
```

**Request body:**

```json
{
  "id": "compress-episodic",
  "condition": {
    "type": "min_entries",
    "namespace": "agent-42",
    "threshold": 500
  },
  "action": {
    "type": "webhook",
    "url": "https://example.com/webhook",
    "headers": {
      "Authorization": "Bearer token"
    }
  },
  "cooldown_ms": 60000
}
```

**Response:** `200 OK` with trigger info, or `409 Conflict` if ID already exists.

### List Triggers

```
GET /trigger
```

**Response:** `200 OK`

```json
{
  "triggers": [
    {
      "id": "compress-episodic",
      "condition": { "type": "min_entries", "namespace": "agent-42", "threshold": 500 },
      "action": { "type": "webhook", "url": "https://example.com/webhook", "headers": {} },
      "enabled": true,
      "cooldown_ms": 60000,
      "last_fired_ms": null
    }
  ]
}
```

### Delete a Trigger

```
DELETE /trigger/{id}
```

**Response:** `200 OK` or `404 Not Found`

### Manually Fire a Trigger

```
POST /trigger/{id}/fire
```

Checks the condition and fires if met, ignoring cooldown. Returns matching entry count.

**Response:** `200 OK`

```json
{
  "ok": true,
  "matching_entries": 523,
  "message": "trigger fired, webhook dispatched"
}
```

### Enable/Disable a Trigger

```
POST /trigger/{id}/enable
POST /trigger/{id}/disable
```

**Response:** `200 OK` with updated trigger info, or `404 Not Found`

## RESP Commands

### TRIGGER ADD

```
TRIGGER ADD <id> <condition_json> [cooldown_ms]
```

Register a trigger with a JSON condition. Note: webhook actions can only be configured via HTTP API.

Example:
```
TRIGGER ADD compress-episodic '{"type":"min_entries","namespace":"agent-42","threshold":500}' 60000
```

### TRIGGER DEL

```
TRIGGER DEL <id>
```

Remove a trigger. Returns `OK` or `ERR trigger not found`.

### TRIGGER LIST

```
TRIGGER LIST
```

List all registered triggers as RESP arrays.

### TRIGGER FIRE

```
TRIGGER FIRE <id>
```

Manually fire a trigger. Returns the count of matching entries, or 0 if condition not met.

### TRIGGER ENABLE / TRIGGER DISABLE

```
TRIGGER ENABLE <id>
TRIGGER DISABLE <id>
```

Enable or disable a trigger. Returns `OK` or `ERR trigger not found`.

### TRIGGER INFO

```
TRIGGER INFO <id>
```

Get detailed info about a specific trigger.

## Cooldown

Each trigger has a cooldown period (default 60 seconds). After a trigger fires, it won't fire again until the cooldown has elapsed. This prevents trigger storms on hot namespaces.

The manual fire endpoint (`POST /trigger/{id}/fire` and `TRIGGER FIRE`) ignores cooldown - it always fires if the condition is met and the trigger is enabled.

## Integration with Relevance Decay

Triggers complement the relevance decay system:

- **Decay + reap**: Low-relevance entries are automatically deleted by `REAPLOW` or the decay reap sweep
- **Triggers**: Before deletion, you can summarize old entries into denser semantic memories

A typical pattern: set a `max_age` trigger at a lower threshold than the decay floor, so entries get summarized before they're reaped. For example:
- Trigger: `max_age` 48 hours -> summarize into semantic entries
- Decay floor: relevance < 0.01 after ~72 hours -> auto-reap

## Webhook Payload

When a trigger fires with a webhook action, it POSTs JSON:

```json
{
  "trigger_id": "compress-episodic",
  "namespace": "agent-42",
  "condition": { "type": "min_entries", "namespace": "agent-42", "threshold": 500 },
  "matching_entries": [
    {
      "key": "agent-42:obs:1",
      "value": "saw a red car",
      "memory_type": "episodic",
      "relevance": 0.35,
      "created_at_ms": 1713000000000,
      "access_count": 2
    }
  ],
  "fired_at_ms": 1713024000000
}
```

The webhook endpoint should return a 2xx status. Non-2xx responses are logged as warnings.
