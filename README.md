<div align="center">
  <img src="resources/basalt_logo_scaled.png" alt="Basalt" width="600">
</div>

# Basalt

A single-binary memory stack for AI platforms. Purpose-built KV store with semantic search, consolidation, and dual-protocol access - no external dependencies, no Redis, no vector DB needed.

**One binary. Two protocols. Zero dependencies.**

Dual-protocol: **HTTP REST API** (port 7380) for agents + **RESP2** (port 6380) for Redis-compatible tooling and benchmarks.

## Why Basalt?

AI agent platforms need memory that general-purpose caches don't provide. Redis and Memcached are key-value stores. Pinecone and Weaviate are vector DBs. Basalt is the **memory layer** - purpose-built for how AI agents actually use memory, in a single binary with no external services.

| What AI platforms need | What Basalt provides |
|---|---|
| Fast key-value storage for observations, facts, and skills | Sharded papaya HashMap with 323ns GET latency |
| Memory that expires when it should | Three memory types: episodic (auto-TTL), semantic (permanent), procedural (permanent) |
| Semantic similarity search | Per-namespace HNSW indexes with lazy rebuild |
| Multi-agent isolation | Namespace scoping with bearer token auth per namespace |
| Memory consolidation (episodic -> semantic) | Built-in promote and compress with LLM-powered summarization |
| Relevance decay (stale memories fade) | Exponential decay with read/write boosts, pinned entries, automatic GC |
| Bulk operations (400+ agents hitting shared memory) | Batch store/get endpoints, pipelined RESP commands |
| Persistence without a separate DB | Binary snapshots with LZ4 compression, auto-rotate |
| High availability | Primary-replica replication with automatic failover |
| Single deployment, no ops overhead | One binary, one config file, no Redis, no vector DB, no external services |

## Quick Start

```bash
# Build and run - that's it
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

# Search by embedding similarity
curl -X POST http://localhost:7380/store/agent-42/search \
  -H 'Content-Type: application/json' \
  -d '{"embedding":[0.1,0.2,0.3,0.4,0.5],"top_k":10}'

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

- **Single binary** - No Redis, no vector DB, no external services. Just `basalt` and a config file.
- **Dual protocol** - HTTP REST API + RESP2 (Redis-compatible)
- **Namespace scoping** - Each agent gets isolated key space at `/store/{namespace}/`
- **Bearer token auth** - Tokens scoped to specific namespaces or wildcard (`*`)
- **Memory types** - Episodic (auto-expiring), semantic (permanent), procedural (permanent)
- **Vector search** - HNSW semantic similarity search on embeddings
- **Persistence** - Binary snapshots with LZ4 compression, CRC32 checksums, auto-rotate last 3
- **Replication** - Asynchronous primary-replica with WAL streaming, automatic failover
- **Compression** - Runtime LZ4 compression for large values (configurable threshold)
- **io_uring** - Linux-only io_uring backend for the RESP server (feature flag)
- **Relevance decay** - Exponential relevance scoring that decays over time; read/write boosts keep hot memories alive, pinned entries stay at 1.0, low-relevance entries GC'd automatically
- **Summarization triggers** - Automatic memory compression when conditions are met (entry count, age, pattern match), with webhook and callback actions
- **Memory consolidation** - Promote episodic memories to semantic, compress related episodes, configurable rules with conflict policies and summary strategies
- **LLM integration** - Optional background LLM inference for summarization, consolidation, and relevance scoring (OpenAI, Anthropic, or custom endpoints)
- **Cross-agent sharing** - Grant one namespace access to another namespace's keys with permission scoping
- **TLS** - Feature-gated TLS for RESP protocol (rustls or native-tls)
- **Prometheus metrics** - Optional `/metrics` endpoint (feature flag)

## Memory Consolidation

Consolidation is the process of promoting episodic memories to semantic ones and compressing related episodes into summarized entries. Without it, an agent's episodic store grows unbounded and fills with low-value observations. Consolidation keeps the store lean while preserving learned knowledge.

### How it works

- **Promote** - An episodic memory that meets a rule's conditions is copied into a semantic key. The original episodic entry can be deleted or left to expire via TTL.
- **Compress** - Multiple episodic entries matching a pattern are merged into a single summarized entry (semantic or episodic, depending on configuration). The source entries are removed after compression succeeds.

### Configuration

Add a `[consolidation]` table to your TOML config:

```toml
[consolidation]
# How often the background sweep runs (milliseconds). 0 disables automatic consolidation.
consolidation_interval_ms = 60_000

[[consolidation.rules]]
type = "promote"
namespace = "agent-42"
pattern = "obs:*"          # keys matching this glob
target_type = "semantic"   # destination memory type
conflict = "version"       # skip | overwrite | version
summary = "concat"         # concat | dedupe | llm_summary

[[consolidation.rules]]
type = "compress"
namespace = "agent-42"
pattern = "chat:*"
min_count = 5              # compress only when this many entries match
target_type = "semantic"
conflict = "skip"
summary = "llm_summary"
```

### Promote rule example

Promote any episodic observation prefixed `obs:` into a permanent semantic fact:

```toml
[[consolidation.rules]]
type = "promote"
namespace = "agent-42"
pattern = "obs:*"
target_type = "semantic"
conflict = "version"
summary = "concat"
```

When the sweep runs, each matching episodic key `obs:7` is promoted to a semantic key with the same value. If a semantic key already exists, the `version` conflict policy appends a version suffix (`obs:7#v2`).

### Compress rule example

Compress chat transcript fragments into a single summary once there are at least 10:

```toml
[[consolidation.rules]]
type = "compress"
namespace = "agent-42"
pattern = "chat:*"
min_count = 10
target_type = "semantic"
conflict = "overwrite"
summary = "llm_summary"
```

All matching episodic entries are collected, their values are merged using the configured summary strategy, and the result is stored as one semantic entry. The source entries are then deleted.

### Conflict policies

When a target key already exists, the conflict policy decides what to do:

| Policy | Behavior |
|---|---|
| **skip** | Do not write; leave the existing entry untouched |
| **overwrite** | Replace the existing entry with the new value |
| **version** | Write to a versioned key (e.g. `fact:earth#v2`) so no data is lost |

### Summary strategies

The summary strategy controls how multiple values are merged during compression (and how a single value is prepared during promotion if needed):

| Strategy | Behavior |
|---|---|
| **concat** | Concatenate all values with a newline separator |
| **dedupe** | Concatenate and remove duplicate lines |
| **llm_summary** | Send values to the configured LLM endpoint and store the returned summary |

The `llm_summary` strategy requires LLM integration to be configured (see [docs/llm.md](docs/llm.md)).

### HTTP API

```bash
# Run consolidation on a namespace (type defaults to "promote" if omitted)
curl -X POST http://localhost:7380/consolidate/agent-42?type=promote
curl -X POST http://localhost:7380/consolidate/agent-42?type=compress

# Check consolidation status for all namespaces
curl http://localhost:7380/consolidate/status
```

### RESP API

```
CONSOLIDATE <namespace> [promote|compress]
CONSOLIDATE STATUS
```

Example:

```
> CONSOLIDATE agent-42 promote
:3          # integer reply: number of entries promoted

> CONSOLIDATE agent-42 compress
:1          # integer reply: number of compressed groups

> CONSOLIDATE STATUS
*2          # array: one entry per namespace
*3
$8
agent-42
:3          # last promote count
:1          # last compress count
```

### Background sweep

When `consolidation_interval_ms > 0`, a background task wakes on the configured interval and runs all matching rules for every namespace. The sweep processes entries in small batches and yields cooperatively between batches so it does not block foreground reads or writes. If a sweep is still running when the next interval fires, the new invocation is skipped (no concurrent sweeps for the same namespace).

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
                            |
                    +-------+-------+
                    v       v       v
                 Snapshot  WAL   Expired
                 Loop     Writer  Entry
                            |
                    +-------+-------+
                    v               v
               LlmClient      Consolidation
               (background)   (background)
```

- **Sharding**: Keys hashed to shards via ahash with per-instance random seed
- **papaya**: Lock-free concurrent SwissTable - reads scale linearly with cores
- **TTL**: Per-entry expiry with lazy eviction (checked on read) + background sweep
- **Dual protocol**: Same engine, two frontends - no data duplication
- **Single binary**: Everything compiles into one executable - HTTP, RESP, persistence, replication, vector search, consolidation

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
| Summarization triggers | [docs/triggers.md](docs/triggers.md) |
| LLM inference | [docs/llm.md](docs/llm.md) |
| Benchmarks & tuning | [docs/performance.md](docs/performance.md) |
| Building, Docker, systemd | [docs/deployment.md](docs/deployment.md) |
| Testing & contributing | [docs/development.md](docs/development.md) |

## License

MIT
