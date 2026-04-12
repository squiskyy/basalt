# Basalt

Ultra-high performance key-value store purpose-built for AI agent memory.

Dual-protocol: **HTTP REST API** (port 7380) for agents + **RESP2** (port 6380) for Redis-compatible tooling and benchmarks.

## Why Basalt?

Redis and Memcached are general-purpose caches. Basalt is laser-focused on one workload: **storing and retrieving memories for AI agents**.

That workload has specific properties we exploit:

| Property | Implication |
|---|---|
| Read-heavy (50-100:1) | Lock-free reads via papaya concurrent HashMap |
| Namespace-partitioned | First-class `/store/{namespace}` paths, native prefix scan |
| TTL-aware by type | Episodic memories auto-expire, semantic/procedural don't |
| Small-to-medium values | No blob overhead, optimized for 16B–8KB values |
| Bulk retrieval | One call to fetch all memories for an agent |

## Performance

Single-threaded criterion benchmarks on the core engine (64 shards, papaya HashMap):

| Operation | Latency | Throughput (est.) |
|---|---|---|
| **GET** (100K keys) | 323 ns | ~3.1M ops/sec |
| **SET** (16B value) | 1.26 µs | ~790K ops/sec |
| **SET** (1KB value) | 2.68 µs | ~370K ops/sec |
| **SET** (8KB value) | 4.11 µs | ~240K ops/sec |
| **Mixed 90/10** (read-heavy) | 584 ns | ~1.7M ops/sec |
| **Namespace scan** (1K keys) | — | — |

Multi-threaded throughput scales linearly with shards (64 by default) since reads are lock-free.

Run your own: `cargo bench`

## Quick Start

```bash
# Build and run
cargo run --release

# HTTP API (port 7380)
curl http://localhost:7380/health
# → {"status":"ok"}

# Store an episodic memory (auto-expires in 1 hour)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"obs:1","value":"saw a red car","type":"episodic","ttl_ms":3600000}'
# → {"key":"obs:1","value":"","type":"episodic","ttl_ms":3600000}

# Retrieve it
curl http://localhost:7380/store/agent-42/obs:1
# → {"key":"obs:1","value":"saw a red car","type":"episodic","ttl_ms":3599420}

# List all memories for an agent
curl http://localhost:7380/store/agent-42

# Filter by type
curl 'http://localhost:7380/store/agent-42?type=episodic'

# Store a semantic memory (permanent — no TTL)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"fact:earth","value":"earth is round","type":"semantic"}'

# Delete a memory
curl -X DELETE http://localhost:7380/store/agent-42/obs:1

# Nuke an entire namespace
curl -X DELETE http://localhost:7380/store/agent-42

# RESP (Redis-compatible) on port 6380
redis-cli -p 6380 PING
# → PONG
redis-cli -p 6380 SET mykey myvalue
# → OK
redis-cli -p 6380 GET mykey
# → "myvalue"
```

## HTTP API

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Health check |
| `GET` | `/info` | Server info (version, shard count) |
| `POST` | `/store/{namespace}` | Store a memory |
| `GET` | `/store/{namespace}` | List all memories in namespace |
| `GET` | `/store/{namespace}/{key}` | Get a specific memory |
| `DELETE` | `/store/{namespace}/{key}` | Delete a memory |
| `DELETE` | `/store/{namespace}` | Delete entire namespace |

### POST /store/{namespace}

```json
{
  "key": "obs:1",
  "value": "saw a red car",
  "type": "episodic",
  "ttl_ms": 3600000
}
```

- `key` (required) — key within the namespace
- `value` (required) — the memory content
- `type` (optional, default: `semantic`) — `episodic`, `semantic`, or `procedural`
- `ttl_ms` (optional) — custom TTL in milliseconds; overrides type default

### GET /store/{namespace}

Query params:
- `type` — filter by memory type (`episodic`, `semantic`, `procedural`)
- `prefix` — filter by key prefix within the namespace

### Response format

```json
{
  "key": "obs:1",
  "value": "saw a red car",
  "type": "episodic",
  "ttl_ms": 3599420
}
```

`ttl_ms` is `null` for semantic and procedural memories (no expiry).

## RESP Commands

Standard Redis-compatible:

| Command | Description |
|---|---|
| `SET key value [EX sec \| PX ms]` | Store a value |
| `GET key` | Retrieve a value |
| `DEL key [key ...]` | Delete keys |
| `MGET key [key ...]` | Multi-get |
| `MSET key value [key value ...]` | Multi-set |
| `KEYS prefix*` | List keys matching prefix |
| `PING` | Health check |
| `INFO` | Server info |

Basalt-specific:

| Command | Description |
|---|---|
| `MSETT key value type [PX ms]` | Set with memory type (`episodic`, `semantic`, `procedural`) |
| `MGETT key` | Get value + type + TTL |
| `MSCAN prefix` | Scan all key-value pairs matching prefix |
| `MTYPE key` | Get the memory type of a key |

## Memory Types

| Type | Description | Default TTL |
|---|---|---|
| **Episodic** | Conversations, observations, events | 1 hour |
| **Semantic** | Facts, rules, learned knowledge | No expiry |
| **Procedural** | Skills, how-to knowledge | No expiry |

Episodic memories auto-expire because old observations become stale. Semantic and procedural memories persist — facts and skills don't go bad.

## Architecture

```
                    ┌─────────────┐
    HTTP ──────────►│   axum      │
  (port 7380)      │   router    │
                    ├─────────────┤
                    │  Command    │────►  Sharded KV Engine
    RESP ──────────►│  Dispatch   │       (64 papaya HashMaps)
  (port 6380)      │             │
                    └─────────────┘
```

- **Sharding**: Keys hashed to shards via fxhash (fast, good distribution)
- **papaya**: Lock-free concurrent SwissTable — reads scale linearly with cores
- **TTL**: Per-entry expiry with lazy eviction (checked on read)
- **Dual protocol**: Same engine, two frontends — no data duplication

## Configuration

```bash
basalt [OPTIONS]

Options:
  --http-host <HOST>     HTTP bind address [default: 127.0.0.1]
  --http-port <PORT>     HTTP port [default: 7380]
  --resp-host <HOST>     RESP bind address [default: 127.0.0.1]
  --resp-port <PORT>     RESP port [default: 6380]
  --shards <N>           Number of shards [default: 64]
```

## 400 Agents? No Problem.

Each agent gets its own namespace (`/store/agent-42/`). The sharded engine distributes keys across 64 independent HashMaps — no global lock, no contention between agents. 400 concurrent HTTP connections is trivial for Tokio's async runtime.

## Roadmap

- [ ] Persistence — async mmap snapshots
- [ ] Vector search — HNSW index for semantic memory embeddings
- [ ] Auth — bearer tokens, namespace-level permissions
- [ ] Replication — primary-replica async replication
- [ ] io_uring — tokio-uring for lower RESP latency
- [ ] SIMD RESP parsing — vectorized protocol parsing
- [ ] Compression — LZ4/zstd for values > 1KB

## License

MIT
