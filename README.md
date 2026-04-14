<div align="center">
  <img src="resources/basalt_logo_scaled.png" alt="Basalt" width="600">
</div>

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
| Small-to-medium values | No blob overhead, optimized for 16B-8KB values |
| Bulk retrieval | One call to fetch all memories for an agent |

## Quick Start

```bash
# Build and run
cargo run --release

# HTTP API (port 7380)
curl http://localhost:7380/health
# -> {"status":"ok"}

# Store an episodic memory (auto-expires in 1 hour)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"obs:1","value":"saw a red car","type":"episodic","ttl_ms":3600000}'

# Retrieve it
curl http://localhost:7380/store/agent-42/obs:1
# -> {"key":"obs:1","value":"saw a red car","type":"episodic","ttl_ms":3599420}

# Store a semantic memory (permanent)
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{"key":"fact:earth","value":"earth is round","type":"semantic"}'

# RESP (Redis-compatible) on port 6380
redis-cli -p 6380 PING
# -> PONG
redis-cli -p 6380 SET mykey myvalue
# -> OK
```

## Performance

Single-threaded criterion benchmarks on the core engine (64 shards, papaya HashMap):

| Operation | Latency | Throughput (est.) |
|---|---|---|
| **GET** (100K keys) | 325 ns | ~3.1M ops/sec |
| **SET** (16B value) | 1.40 us | ~710K ops/sec |
| **SET** (1KB value) | 3.57 us | ~280K ops/sec |
| **SET** (8KB value) | 4.39 us | ~1.7 GB/s |
| **Mixed 90/10** (read-heavy) | 535 ns | ~1.9M ops/sec |

RESP2 protocol parser (SIMD-accelerated via memchr):

| Operation | Latency |
|---|---|
| **Parse simple string** | 86 ns |
| **Parse SET command** | 193 ns |
| **Parse 10-command pipeline** | 3.18 us (318 ns/cmd) |
| **Parse 100-command pipeline** | 39.7 us (397 ns/cmd) |
| **Serialize bulk string** | 58 ns |

Multi-threaded throughput scales linearly with shards (64 by default) since reads are lock-free.

Run your own: `cargo bench`

## Memory Types

| Type | Description | Default TTL |
|---|---|---|
| **Episodic** | Conversations, observations, events | 1 hour |
| **Semantic** | Facts, rules, learned knowledge | No expiry |
| **Procedural** | Skills, how-to knowledge | No expiry |

Episodic memories auto-expire because old observations become stale. Semantic and procedural memories persist - facts and skills don't go bad.

## Features

- **Dual protocol** - HTTP REST API + RESP2 (Redis-compatible)
- **Namespace scoping** - Each agent gets isolated key space at `/store/{namespace}/`
- **Bearer token auth** - Tokens scoped to specific namespaces or wildcard (`*`)
- **Memory types** - Episodic (auto-expiring), semantic (permanent), procedural (permanent)
- **Vector search** - HNSW semantic similarity search on embeddings
- **Persistence** - Binary snapshots with LZ4 compression, auto-rotate last 3
- **Replication** - Asynchronous primary-replica with WAL streaming
- **Compression** - Runtime LZ4 compression for large values (configurable threshold)
- **io_uring** - Linux-only io_uring backend for the RESP server (feature flag)
- **Relevance decay** - Exponential relevance scoring that decays over time; read/write boosts keep hot memories alive, pinned entries stay at 1.0, low-relevance entries GC'd automatically
- **LLM integration** - Optional background LLM inference for summarization, consolidation, and relevance scoring (OpenAI, Anthropic, or custom endpoints)

## Architecture

```
                    +-------------+
    HTTP ---------->|   axum      |
  (port 7380)      |   router    |
                    +-------------+
                    |  Command    |---->  Sharded KV Engine
    RESP ---------->|  Dispatch   |       (64 papaya HashMaps)
  (port 6380)      |             |
                    +-------------+
```

- **Sharding**: Keys hashed to shards via ahash with per-instance random seed
- **papaya**: Lock-free concurrent SwissTable - reads scale linearly with cores
- **TTL**: Per-entry expiry with lazy eviction (checked on read) + background sweep
- **Dual protocol**: Same engine, two frontends - no data duplication

## Configuration

CLI flags, TOML config file, or both (CLI overrides file):

```bash
# Minimal
basalt

# With config file
basalt --config /etc/basalt/basalt.toml

# With persistence + auth
basalt --db-path /var/lib/basalt --auth "bsk-admin:*"
```

See `basalt.example.toml` for all options. Full reference: [docs/configuration.md](docs/configuration.md).

## Documentation

| Topic | File |
|---|---|
| Architecture & internals | [docs/architecture.md](docs/architecture.md) |
| HTTP + RESP API reference | [docs/api-reference.md](docs/api-reference.md) |
| All config options | [docs/configuration.md](docs/configuration.md) |
| Memory types & TTL | [docs/memory-types.md](docs/memory-types.md) |
| Auth & namespace scoping | [docs/auth.md](docs/auth.md) |
| Snapshots & persistence | [docs/persistence.md](docs/persistence.md) |
| Primary-replica replication | [docs/replication.md](docs/replication.md) |
| Vector search | [docs/vector-search.md](docs/vector-search.md) |
| LLM inference | [docs/llm.md](docs/llm.md) |
| Benchmarks & tuning | [docs/performance.md](docs/performance.md) |
| Building, Docker, systemd | [docs/deployment.md](docs/deployment.md) |
| Testing & contributing | [docs/development.md](docs/development.md) |

## License

MIT
