# API Reference

Basalt exposes two protocols: an HTTP REST API (port 7380) for agents and a RESP2 protocol (port 6380) compatible with Redis clients. Both access the same memory store - no external services needed.

## HTTP REST API

Base URL: `http://localhost:7380`

### Health Check

```
GET /health
```

Returns server health status. No authentication required.

**Response:** `200 OK`

```json
{"status": "ok"}
```

### Server Info

```
GET /info
```

Returns server metadata. No authentication required.

**Response:** `200 OK`

```json
{
  "version": "0.2.0",
  "shard_count": 64,
  "role": "primary",
  "connected_replicas": 0,
  "replication_offset": 0,
  "wal_size": 10000
}
```

### Store a Memory

```
POST /store/{namespace}
```

Store a single memory in the given namespace. Requires auth if enabled.

**Request body:**

```json
{
  "key": "obs:1",
  "value": "saw a red car",
  "type": "episodic",
  "ttl_ms": 3600000
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `key` | Yes | Key within the namespace |
| `value` | Yes | The memory content |
| `type` | No | Memory type: `episodic`, `semantic`, or `procedural` (default: `semantic`) |
| `ttl_ms` | No | Custom TTL in milliseconds; overrides type default |
| `embedding` | No | Float array for semantic similarity search |

**Response:** `200 OK`

```json
{
  "key": "obs:1",
  "value": "",
  "type": "episodic",
  "ttl_ms": 3600000
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| `401` | Missing or invalid auth token |
| `403` | Token does not have access to this namespace |
| `429` | Rate limit exceeded (when rate limiting is enabled) |
| `507` | Shard is at capacity (`max_entries` exceeded) |

### Batch Store

```
POST /store/{namespace}/batch
```

Store multiple memories in a single request. Requires auth if enabled.

**Request body:**

```json
{
  "memories": [
    {"key": "obs:1", "value": "saw a red car", "type": "episodic", "ttl_ms": 3600000},
    {"key": "fact:x", "value": "something", "type": "semantic"},
    {"key": "skill:deploy", "value": "run cargo build --release", "type": "procedural"}
  ]
}
```

**Response:** `200 OK`

```json
{"ok": true, "stored": 3}
```

If any individual memory hits the capacity limit, it is silently skipped. The `stored` count reflects how many were actually inserted.

### Batch Get

```
POST /store/{namespace}/batch/get
```

Retrieve multiple memories by key in a single request. Requires auth if enabled.

**Request body:**

```json
{"keys": ["obs:1", "fact:gravity", "nonexistent"]}
```

**Response:** `200 OK`

```json
{
  "memories": [
    {"key": "obs:1", "value": "saw a red car", "type": "episodic", "ttl_ms": 3599420},
    {"key": "fact:gravity", "value": "earth is round", "type": "semantic"}
  ],
  "missing": ["nonexistent"]
}
```

Expired keys appear in `missing` (not `memories`).

### List Memories

```
GET /store/{namespace}
```

List all memories in a namespace. Requires auth if enabled.

**Query parameters:**

| Param | Description |
|-------|-------------|
| `type` | Filter by memory type: `episodic`, `semantic`, or `procedural` |
| `prefix` | Filter by key prefix within the namespace |

**Examples:**

```bash
# All memories for agent-42
curl http://localhost:7380/store/agent-42

# Only episodic memories
curl 'http://localhost:7380/store/agent-42?type=episodic'

# Only memories with key prefix "obs:"
curl 'http://localhost:7380/store/agent-42?prefix=obs:'
```

**Response:** `200 OK`

```json
[
  {"key": "obs:1", "value": "saw a red car", "type": "episodic", "ttl_ms": 3599420},
  {"key": "fact:earth", "value": "earth is round", "type": "semantic"}
]
```

### Get a Memory

```
GET /store/{namespace}/{key}
```

Retrieve a specific memory. Requires auth if enabled.

**Response:** `200 OK`

```json
{"key": "obs:1", "value": "saw a red car", "type": "episodic", "ttl_ms": 3599420}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| `404` | Key not found (or expired) |

### Delete a Memory

```
DELETE /store/{namespace}/{key}
```

Delete a specific memory. Requires auth if enabled.

**Response:** `200 OK`

```json
{"deleted": true}
```

If the key does not exist:

```json
{"deleted": false}
```

### Delete a Namespace

```
DELETE /store/{namespace}
```

Delete all memories in a namespace (prefix scan + delete). Requires auth if enabled.

**Response:** `200 OK`

```json
{"deleted": 5}
```

### Vector Search

```
POST /store/{namespace}/search
```

Search for memories by embedding similarity in a namespace. Uses HNSW approximate nearest neighbor search. Requires auth if enabled.

**Request body:**

```json
{
  "embedding": [0.1, 0.2, 0.3, ...],
  "top_k": 10
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `embedding` | Yes | Float array to search against |
| `top_k` | No | Number of results (default: 10) |

**Response:** `200 OK`

```json
{
  "results": [
    {
      "key": "fact:gravity",
      "value": "earth is round",
      "type": "semantic",
      "distance": 0.05
    }
  ]
}
```

The index is lazily rebuilt when stale (namespace version mismatch).

### Trigger Snapshot

```
POST /snapshot
```

Manually trigger a persistence snapshot. Requires auth if enabled.

**Response:** `200 OK`

```json
{"ok": true, "path": "/var/lib/basalt/snapshot-1712951000000.bin", "entries": 42}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| `412` | No `db_path` configured (persistence disabled) |

### Response Format

All memory responses use a consistent JSON shape:

```json
{
  "key": "string",
  "value": "string",
  "type": "episodic|semantic|procedural",
  "ttl_ms": 3599420
}
```

`ttl_ms` is `null` for semantic and procedural memories (no expiry). For episodic memories, it shows the remaining TTL in milliseconds.

## RESP2 Protocol

Port 6380. Compatible with Redis clients (redis-cli, Redis libraries, etc.).

### Standard Redis Commands

| Command | Syntax | Description |
|---------|--------|-------------|
| `PING` | `PING` | Health check |
| `SET` | `SET key value [EX sec \| PX ms]` | Store a value |
| `GET` | `GET key` | Retrieve a value |
| `DEL` | `DEL key [key ...]` | Delete one or more keys |
| `MGET` | `MGET key [key ...]` | Multi-get |
| `MSET` | `MSET key value [key value ...]` | Multi-set |
| `KEYS` | `KEYS prefix*` | List keys matching prefix |
| `INFO` | `INFO` | Server info |
| `AUTH` | `AUTH token` | Authenticate (when auth enabled) |

### Basalt-Specific Commands

| Command | Syntax | Description |
|---------|--------|-------------|
| `MSETT` | `MSETT key value type [PX ms]` | Set with memory type |
| `MGETT` | `MGETT key` | Get value + type + TTL |
| `MSCAN` | `MSCAN prefix` | Scan all key-value pairs matching prefix |
| `MTYPE` | `MTYPE key` | Get the memory type of a key |
| `SNAP` | `SNAP` | Trigger a manual snapshot |
| `VSEARCH` | `VSEARCH namespace embedding [COUNT N]` | Vector similarity search |
| `REPLICAOF` | `REPLICAOF host port` | Become a replica of primary |
| `REPLICAOF NO ONE` | `REPLICAOF NO ONE` | Stop replicating, become primary |

### MSETT - Set with Memory Type

```
MSETT agent-42:obs:1 "saw a red car" episodic PX 3600000
```

Parameters:
- `key` - full key (include namespace prefix)
- `value` - the value
- `type` - `episodic`, `semantic`, or `procedural`
- `PX ms` - optional TTL in milliseconds

Response: `+OK`

### MGETT - Get with Type and TTL

```
MGETT agent-42:obs:1
```

Response: Array with `[value, type, ttl_ms]`

```
*3
$12
saw a red car
$9
episodic
:3599420
```

If the key does not exist: `$-1` (null bulk string)

### MSCAN - Prefix Scan

```
MSCAN agent-42:
```

Returns all key-value pairs where the key starts with the given prefix.

Response: Array of `[key, value]` pairs.

### VSEARCH - Vector Similarity Search

```
VSEARCH agent-42 [0.1,0.2,0.3] COUNT 5
```

Parameters:
- `namespace` - namespace to search within
- `embedding` - JSON array of floats
- `COUNT N` - optional, number of results (default: 10)

Response: Array of `[key, value, distance]` triples.

### REPLICAOF - Replication

```
REPLICAOF 10.0.1.5 6380
```

Tells this node to become a replica of the specified primary. The node will:
1. Connect to the primary
2. Receive a full resync (snapshot)
3. Stream WAL entries for ongoing replication

```
REPLICAOF NO ONE
```

Stops replicating and promotes this node back to a primary.

### Pipelining

The RESP server supports command pipelining. Send multiple commands without waiting for responses:

```
*1\r\n$4\r\nPING\r\n*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n
```

All responses are batched and sent in a single write.

### Error Responses

Standard RESP error format:

```
-ERR <message>
```

Common errors:

| Error | Condition |
|-------|-----------|
| `-NOAUTH Authentication required` | Auth enabled but not authenticated |
| `-ERR invalid token` | AUTH with wrong token |
| `-ERR max entries exceeded` | Shard at capacity |
| `-ERR no db_path configured` | SNAP without persistence |
| `-ERR unknown command` | Unrecognized command |
| `-ERR rate limit exceeded` | Per-connection rate limit exceeded |

### Rate Limiting

When rate limiting is enabled (via `--rate-limit-requests` or `rate_limit_requests` in config), the RESP server applies per-connection rate limiting. Each TCP connection is tracked independently. When a connection exceeds the configured request limit within the window, subsequent commands receive:

```
-ERR rate limit exceeded
```

AUTH and REPLICAOF commands are exempt from rate limiting.
