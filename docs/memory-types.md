# Memory Types

Basalt's memory type system maps directly to how AI agents organize information. Unlike a generic KV store where everything is a string with optional TTL, Basalt understands that observations expire, facts persist, and skills endure. Three types - episodic, semantic, and procedural - each with different TTL behavior.

## The Three Types

### Episodic

Observations, conversations, events. Things that happened at a specific point in time.

```
"I saw a red car at the intersection at 3pm"
"The user asked about database performance"
"The deployment succeeded at 14:32 UTC"
```

**Default TTL: 1 hour (3,600,000 ms)**

Episodic memories auto-expire because old observations become stale. The exact moment you saw a red car is useful now, but irrelevant in a day.

### Semantic

Facts, rules, learned knowledge. Things that are true regardless of time.

```
"Earth is round"
"PostgreSQL uses MVCC for concurrency"
"The API rate limit is 100 requests per minute"
```

**Default TTL: None (permanent)**

Semantic memories persist because facts don't go bad. "Earth is round" was true yesterday, is true today, and will be true tomorrow.

### Procedural

Skills, how-to knowledge. Step-by-step procedures.

```
"To deploy, run cargo build --release"
"To reset the database, use DELETE /store/{namespace}"
"The incident response checklist: 1) verify, 2) mitigate, 3) communicate"
```

**Default TTL: None (permanent)**

Procedural memories persist because skills are durable. Once you know how to deploy, that knowledge stays relevant.

## TTL Behavior

| Type | Default TTL | Custom TTL | Expires At |
|------|------------|------------|------------|
| Episodic | 3,600,000 ms (1 hour) | Override with `ttl_ms` | `now + ttl_ms` |
| Semantic | None (permanent) | Override with `ttl_ms` | `now + ttl_ms` if provided |
| Procedural | None (permanent) | Override with `ttl_ms` | `now + ttl_ms` if provided |

Key behaviors:

- **Custom TTL overrides type default**: You can set `ttl_ms` on any type to override the default
- **None means no expiry**: Semantic and procedural with no `ttl_ms` never expire
- **Lazy eviction**: Expired entries are cleaned up on `get()` (checked at read time)
- **Background sweep**: A configurable sweep interval (`--sweep-interval`) proactively removes expired entries that aren't being read
- **TTL in responses**: The `ttl_ms` field shows remaining time, not the original value

## Usage Examples

### HTTP

```bash
# Episodic memory with default 1-hour TTL
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"obs:1","value":"saw a red car","type":"episodic"}'

# Episodic memory with custom 5-minute TTL
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"obs:2","value":"user said hello","type":"episodic","ttl_ms":300000}'

# Semantic memory (permanent)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"fact:earth","value":"earth is round","type":"semantic"}'

# Procedural memory (permanent)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"skill:deploy","value":"run cargo build --release","type":"procedural"}'

# Semantic memory with a custom 24-hour TTL (temporary fact)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"fact:temp","value":"server is in maintenance mode","type":"semantic","ttl_ms":86400000}'
```

### RESP

```
# Episodic with default TTL
MSETT agent-42:obs:1 "saw a red car" episodic

# Episodic with custom TTL (5 minutes)
MSETT agent-42:obs:2 "user said hello" episodic PX 300000

# Semantic (permanent)
MSETT agent-42:fact:earth "earth is round" semantic

# Procedural (permanent)
MSETT agent-42:skill:deploy "run cargo build --release" procedural
```

## Retrieval with Type Information

### HTTP

```bash
# Get a specific memory (includes type and TTL)
curl http://localhost:7380/store/agent-42/obs:1
# → {"key":"obs:1","value":"saw a red car","type":"episodic","ttl_ms":3599420}

# Filter by type
curl 'http://localhost:7380/store/agent-42?type=episodic'
curl 'http://localhost:7380/store/agent-42?type=semantic'
curl 'http://localhost:7380/store/agent-42?type=procedural'
```

### RESP

```
# Get value + type + TTL
MGETT agent-42:obs:1
# → Array: ["saw a red car", "episodic", 3599420]

# Get only the type
MTYPE agent-42:obs:1
# → "episodic"
```

## Design Rationale

### Why not just use TTL everywhere?

You could implement the same behavior by always specifying `ttl_ms`, but memory types provide:

1. **Sensible defaults**: No need to remember to set TTL on every episodic write
2. **Self-documenting data**: The type tells you what kind of information this is
3. **Filtering**: Query all episodic memories (recent observations) vs semantic (facts) vs procedural (skills)
4. **Agent-friendly**: Maps to cognitive science models of memory that AI agents use

### Why 1 hour for episodic?

One hour is a reasonable default for "short-term observation" in agent workflows:
- Long enough to be useful within a session
- Short enough to prevent stale data from accumulating
- Can be overridden with `ttl_ms` for longer or shorter observations

### Why is lazy eviction sufficient?

For AI memory workloads:
- Most episodic entries are written once and read a few times within their TTL
- When read after expiry, they're correctly filtered out
- The background sweeper (`--sweep-interval`) catches entries that are never read again
- Combined, these keep memory usage bounded without the complexity of timer-based expiry
