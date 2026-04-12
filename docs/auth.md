# Authentication

Basalt supports optional bearer token authentication scoped to namespaces. When no tokens are configured, all requests are allowed (auth disabled).

## Overview

- Tokens are scoped to specific namespaces or all namespaces (wildcard)
- Auth is checked per-request in HTTP (axum middleware) and per-connection in RESP
- When auth is disabled (no tokens configured), all requests pass through
- Both CLI flags and a tokens file are supported

## Token Format

### CLI Format

```bash
--auth "TOKEN:NAMESPACE1,NAMESPACE2"
```

- Colon separates the token from its namespaces
- Commas separate multiple namespaces
- `*` grants access to all namespaces (admin/wildcard)

Examples:

```bash
# Admin token with access to everything
--auth "bsk-admin-secret:*"

# Agent token scoped to two namespaces
--auth "bsk-agent1-abc123:agent-1,shared"

# Multiple tokens
basalt --auth "bsk-admin:*" --auth "bsk-agent1:agent-1,shared" --auth "bsk-agent2:agent-2"
```

### File Format

One token per line, whitespace-delimited:

```
# Comments with #
bsk-admin-secret *
bsk-agent1-abc123 agent-1 shared
bsk-agent2-def456 agent-2
```

- Whitespace (not colons) separates token from namespaces
- Spaces (not commas) separate multiple namespaces
- `#` lines are comments, blank lines are ignored

Specify the file path:

```bash
basalt --auth-file /etc/basalt/tokens.txt
```

Or in TOML:

```toml
[auth]
tokens_file = "/etc/basalt/tokens.txt"
```

### Merge Behavior

CLI `--auth` flags override file entries with the same token value. This lets you override a specific token without editing the file:

```bash
# File has bsk-agent1 with namespace "agent-1"
# CLI overrides it with "agent-1,shared,metrics"
basalt --auth-file /etc/basalt/tokens.txt --auth "bsk-agent1:agent-1,shared,metrics"
```

## AuthStore

The `AuthStore` uses a papaya concurrent HashMap for lock-free token lookups:

```rust
pub struct AuthStore {
    tokens: HashMap<String, TokenInfo>,  // papaya lock-free HashMap
}

pub struct TokenInfo {
    pub namespaces: Vec<String>,  // ["*"] for wildcard
}
```

### Authorization Logic

```rust
pub fn is_authorized(&self, token: &str, namespace: &str) -> bool {
    let pinned = self.tokens.pin();
    if pinned.is_empty() { return true; }  // No tokens = auth disabled
    pinned.get(token)
        .map(|t| t.namespaces.iter().any(|ns| ns == "*" || ns == namespace))
        .unwrap_or(false)
}
```

Critical: when the token map is empty, `is_authorized()` returns `true` for ALL inputs. This ensures backward compatibility - adding auth is opt-in.

### Namespace Extraction (HTTP)

The auth middleware extracts the namespace from the URL path:

```rust
fn extract_namespace_from_path(path: &str) -> Option<&str> {
    let stripped = path.strip_prefix("/store/")?;
    let end = stripped.find('/').unwrap_or(stripped.len());
    Some(&stripped[..end])
}
```

- `/store/agent-42/obs:1` -> namespace = `agent-42`
- `/store/agent-42` -> namespace = `agent-42`
- `/store/` -> None (no namespace = denied)

## HTTP Authentication

### Public Endpoints (no auth required)

- `GET /health`
- `GET /info`

### Protected Endpoints (auth required when enabled)

All `/store/*` and `/snapshot` endpoints require `Authorization: Bearer <token>` when auth is enabled.

### Auth Flow

1. Middleware extracts `Authorization: Bearer <token>` header
2. If no header and auth is enabled: return `401 Unauthorized`
3. If header present but token is invalid: return `401 Unauthorized`
4. If token is valid but not authorized for the namespace: return `403 Forbidden`
5. If auth is disabled (no tokens configured): pass through

### Example

```bash
# Works - admin has access to everything
curl -H "Authorization: Bearer bsk-admin-secret" \
  http://localhost:7380/store/agent-1/mem:1

# Works - agent1 has access to agent-1 namespace
curl -H "Authorization: Bearer bsk-agent1-abc123" \
  -X POST http://localhost:7380/store/agent-1 \
  -d '{"key":"obs:1","value":"saw something","type":"episodic"}'

# 403 - agent1 cannot access agent-2 namespace
curl -H "Authorization: Bearer bsk-agent1-abc123" \
  http://localhost:7380/store/agent-2/mem:1

# 401 - no token at all
curl http://localhost:7380/store/agent-1/mem:1
```

## RESP Authentication

When auth is enabled, clients must authenticate before issuing any other command:

```
AUTH bsk-admin-secret    → +OK
AUTH bsk-wrong-token     → -ERR invalid token
PING (no auth)           → -NOAUTH Authentication required
```

### Connection Flow

1. Client connects
2. If auth is enabled, `authenticated = false`
3. Client sends `AUTH <token>`
4. Server validates token and checks namespace scope
5. If valid: `authenticated = true`, client can issue commands
6. If invalid: `-ERR invalid token`, client remains unauthenticated

Note: RESP uses a single auth token per connection (no per-command namespace scoping like HTTP). The token's namespace list determines which keys the connection can access.

### AUTH is Intercepted at Connection Level

AUTH is handled in the connection handler loop, NOT in CommandHandler. This is because AUTH modifies per-connection state (`authenticated` flag) that the handler doesn't own:

```rust
if cmd.name == "AUTH" {
    let resp = handle_auth(&cmd, &auth, &mut authenticated, &mut auth_token);
    responses.push(resp);
    continue;
}
if auth_enabled && !authenticated {
    responses.push(RespValue::Error("NOAUTH Authentication required".to_string()));
    continue;
}
```

## Design Decisions

### Why per-namespace scoping?

AI agents typically each have their own namespace (`/store/agent-42/`). Namespace scoping prevents one compromised agent from reading or modifying another agent's memories. Admin tokens with `*` access can still manage all namespaces.

### Why empty map = allow all?

This makes auth completely opt-in. A Basalt instance started without any `--auth` flags or `--auth-file` behaves as if auth doesn't exist - zero overhead, zero configuration.

### Why different delimiters for CLI vs file?

CLI args are single strings that need explicit separators (colon between token and namespaces, commas between namespaces). Files have line structure where whitespace splitting is natural and more readable.
