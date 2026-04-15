# Architecture

Basalt is a single-binary memory stack for AI platforms. It combines a high-performance KV store, semantic vector search, memory consolidation, and relevance decay into one deployable unit with no external dependencies. This document covers the internal architecture, data flow, and design decisions.

## High-Level Overview

```
                      ┌──────────────┐
   HTTP ─────────────►│   axum       │
  (port 7380)         │   router     │
                      ├──────────────┤
                      │  Command     │────►  Arc<KvEngine>
   RESP ─────────────►│  Dispatch    │       (shared state)
  (port 6380)         │              │
                      └──────────────┘
                              │
                      ┌───────┼───────┐
                      ▼       ▼       ▼
                   Snapshot  WAL   Expired
                   Loop     Writer  Entry
                              │
                      ┌───────┴───────┐
                      ▼               ▼
                 LlmClient      Consolidation
                 (background)   (background)
```

Both protocols share a single `Arc<KvEngine>` instance. There is no data duplication - the HTTP and RESP frontends are thin dispatch layers over the same engine.

## Core Engine

### Sharding

The `KvEngine` partitions data across N shards (default: 64, must be power of 2). Each shard is an independent papaya lock-free concurrent HashMap.

```
KvEngine
├── shards: Vec<Shard>       // 64 shards by default
├── shard_mask: usize        // shard_count - 1 (for fast & bitwise routing)
├── hash_state: RandomState  // ahash with per-instance random seed
├── vector_indexes: Mutex<HashMap<String, HnswIndex>>
└── namespace_versions: Mutex<HashMap<String, AtomicU64>>
```

Key routing:

```rust
fn shard_index(&self, key: &str) -> usize {
    let mut h = self.hash_state.build_hasher();
    key.hash(&mut h);
    h.finish() as usize & self.shard_mask
}
```

- **Bitwise AND** instead of modulo - faster, works because shard count is always a power of 2
- **ahash with random seed** - each engine instance gets a unique seed, preventing hash-flooding attacks

### Why papaya?

papaya provides lock-free reads on a SwissTable data structure. For AI memory workloads with 50-100:1 read-to-write ratios, this is significantly faster than alternatives like dashmap which use fine-grained locks on reads.

### Shard Structure

Each `Shard` contains:

```rust
pub struct Shard {
    data: HashMap<String, Entry>,  // papaya lock-free HashMap
    count: AtomicUsize,            // approximate entry count
    max_entries: usize,            // capacity limit per shard
    compression_threshold: usize,  // LZ4 compression threshold (bytes)
}
```

The `count` field is approximate (AtomicUsize with Relaxed ordering) - a slight over/under-count is acceptable for the capacity guard rail.

### Entry Structure

```rust
pub struct Entry {
    pub value: Vec<u8>,           // raw bytes (may be LZ4-compressed)
    pub compressed: bool,         // whether value is compressed in memory
    pub expires_at: Option<u64>,  // UNIX timestamp in ms, None = no expiry
    pub memory_type: MemoryType,  // episodic, semantic, or procedural
    pub embedding: Option<Vec<f32>>,  // optional vector for semantic search
}
```

Key design points:
- `value` is `Vec<u8>`, not `String` - holds both UTF-8 text and compressed binary
- `compressed` flag enables transparent runtime compression/decompression
- `expires_at` is an absolute timestamp, not a TTL counter - avoids drift

### Capacity Limits (OOM Prevention)

Each shard has a `max_entries` limit (default: 1,000,000). When a shard is at capacity, new inserts that would create a new key are rejected:

- HTTP: 507 Insufficient Storage
- RESP: `-ERR max entries exceeded`

Updates to existing keys always succeed (no count change). The check uses Relaxed ordering - a few extra entries from TOCTOU races are acceptable.

### Expired Entry Sweeper

Expired entries are lazily cleaned on `get()` (check expiry, return None, remove from map). For write-heavy episodic workloads where entries pile up without being read, a background sweeper runs periodically:

1. Iterates each shard with `papaya::pin()`
2. Collects expired keys (two-pass: scan then delete, since papaya doesn't allow mutation during iteration)
3. Removes expired entries and decrements counts
4. Yields between shards with `tokio::task::yield_now()`

Config: `--sweep-interval` (default 30s, 0 = disabled).

## Memory Types

Three types map to how AI agent memory actually works:

| Type | Description | Default TTL | Use Case |
|------|------------|-------------|----------|
| Episodic | Observations, conversations, events | 1 hour | "Saw a red car at 3pm" |
| Semantic | Facts, rules, learned knowledge | None | "Earth is round" |
| Procedural | Skills, how-to knowledge | None | "To deploy, run `cargo build --release`" |

When storing with type `episodic` and no explicit TTL, 3,600,000ms (1 hour) is applied automatically. Semantic and procedural have no expiry by default.

## HTTP Server

Built on axum 0.8 with tokio. Routes are split into public and protected:

```
Public (no auth required):
  GET  /health
  GET  /info

Protected (auth required when enabled):
  POST   /store/{namespace}
  POST   /store/{namespace}/batch
  POST   /store/{namespace}/batch/get
  GET    /store/{namespace}
  GET    /store/{namespace}/{key}
  DELETE /store/{namespace}/{key}
  DELETE /store/{namespace}
  POST   /store/{namespace}/search
  POST   /snapshot
```

### State

```rust
pub struct AppState {
    pub engine: Arc<KvEngine>,
    pub auth: Arc<AuthStore>,
    pub db_path: Option<String>,
    pub compression_threshold: usize,
    pub repl_state: Option<Arc<ReplicationState>>,
}
```

## RESP Server

Custom RESP2 protocol implementation with memchr SIMD-accelerated parsing.

### Parser

The RESP parser uses `memchr::memchr(b'\r', ...)` to find CRLF delimiters. This dispatches to AVX2/SSE2 on x86_64 and NEON on aarch64, making it significantly faster than byte-by-byte scanning.

Benchmark results:
- Parse simple string: 88ns
- Parse SET command: 190ns
- Parse 100-command pipeline: 295ns/cmd

### Buffer Management

- 8KB read buffers (fewer syscalls than 4KB)
- Buffer recycling: if buffer grows past 64KB but is empty, reset to 8KB capacity
- Pipelining: multiple commands in one read are all parsed and responses batched into a single `write_all()`
- Partial commands: track consumed bytes per parse, drain only consumed bytes, keep partial data for next read

### Connection Flow

```
1. Accept TCP connection
2. Read into buffer
3. Parse all complete RESP commands from buffer
4. For each command:
   a. If AUTH and auth enabled → authenticate
   b. If REPLICAOF → handle replication handshake
   c. Otherwise → dispatch to CommandHandler
5. Batch-serialize all responses
6. write_all() back to client
7. Drain consumed bytes from buffer
8. Loop
```

### io_uring Variant (Feature-Gated)

An alternative RESP server using raw `io_uring` is available behind the `io-uring` feature flag. It runs in a dedicated std::thread (not tokio) and uses:

- `slab::Slab` for connection and token tracking (O(1) insert/remove)
- Multishot accept (`AcceptMulti`) for efficient connection acceptance
- Per-connection stable `Box<[u8; 8192]>` read buffers
- Backlog queue for submission queue overflow
- Partial write tracking per connection

Build with: `cargo build --release --features io-uring`

When to use: >10K concurrent connections where the ~1-2µs per-op overhead of epoll matters. For most workloads, the tokio epoll server is already very fast.

## Persistence

### Snapshot Format

**v1 (original):**
```
MAGIC:    b"BASALT\x00" (7 bytes)
VERSION:  u8 (1 byte, value = 1)
COUNT:    u64 LE (8 bytes)

Per entry:
  key_len:    u32 LE
  key:        [u8; key_len]       (UTF-8)
  val_len:    u32 LE
  value:      [u8; val_len]       (raw bytes)
  mem_type:   u8                  (0=episodic, 1=semantic, 2=procedural)
  expires_at: u64 LE              (0 = no expiry, otherwise epoch millis)
```

**v2 (with compression):**
Same header but VERSION = 2. Adds a flags byte per entry:

```
Per entry:
  key_len:    u32 LE
  key:        [u8; key_len]
  flags:      u8                  (bit 0 = compressed, bits 1-7 reserved)
  val_len:    u32 LE
  value:      [u8; val_len]       (LZ4-compressed if flag bit 0 set)
  mem_type:   u8
  expires_at: u64 LE
```

v2 reader can also read v1 snapshots (backward compatible).

### Snapshot Lifecycle

1. **Write**: Serialize all live entries to `.tmp` file, then `fs::rename()` to final path (atomic)
2. **Auto-prune**: Keep last N snapshots (default 3), delete older ones
3. **Naming**: `snapshot-{unix_timestamp_millis}.bin`
4. **Restore**: On startup, find latest snapshot in `db_path`, deserialize, insert via `set_force()` (bypasses capacity checks), skip expired entries
5. **Triggers**: Auto (interval), manual (`POST /snapshot` or `SNAP`), shutdown (final snapshot)

### Compression

Both snapshot and runtime compression use LZ4 via `lz4_flex` (pure Rust, no C dependencies):

- **Runtime**: Values >= `compression_threshold` bytes are compressed in memory. Transparent - decompressed on `get()`, `scan_prefix()`, etc.
- **Snapshot**: Values >= `snapshot_compression_threshold` bytes are compressed in the snapshot file.
- Only compress if it actually reduces size (incompressible data stays uncompressed).
- Config: `--compression-threshold` (runtime, default 1024), `--snapshot-compression-threshold` (snapshot, default 1024), 0 = disabled.

## Replication

Primary-replica async replication via the RESP protocol.

### Write-Ahead Log (WAL)

```
Wal {
    entries: VecDeque<(u64, WalEntry)>,  // circular buffer
    max_size: usize,                       // configurable, default 10000
    notify: Arc<Notify>,                   // wake streaming replicas
}
```

Each `WalEntry` records:
- Operation type: Set, Delete, or DeletePrefix
- Key, value, memory type, TTL, timestamp
- Optional embedding vector

Binary wire format includes sequence numbers for incremental replication.

### Replication Protocol

Over the existing RESP port:

1. Replica sends `REPLICAOF <host> <port>` to primary
2. Primary sends `+FULLRESYNC <seq>\r\n`
3. Primary sends snapshot as RESP bulk arrays
4. Primary sends `+STREAM\r\n`
5. Primary streams WAL entries as they arrive (using `Notify` channel, not polling)
6. Replica applies writes to local engine

If a replica falls behind the WAL's oldest entry, the primary triggers a full resync. Backpressure is applied via a 64KB pending buffer limit per replica.

### Replication State

```rust
pub enum ReplicationRole { Primary, Replica }

pub struct ReplicationState {
    role: RwLock<ReplicationRole>,
    wal: Arc<Wal>,
    engine: Arc<KvEngine>,
    connected_replicas: AtomicU64,
    replication_offset: AtomicU64,
    replica_shutdown: RwLock<Option<watch::Sender<bool>>>,
}
```

## Vector Search

Per-namespace HNSW indexes using the `instant-distance` crate:

- `Entry.embedding: Option<Vec<f32>>` - optional float vector stored with each entry
- Indexes are lazy-rebuilt: a per-namespace version counter tracks changes
- On search, if the index version is stale, it rebuilds from all entries with embeddings in that namespace
- O(n) rebuild cost, but only on stale reads
- HTTP: `POST /store/{namespace}/search` with `{"embedding": [...], "top_k": 10}`
- RESP: `VSEARCH namespace <json-array> [COUNT N]`

## Time Handling

`now_ms()` returns milliseconds since UNIX epoch with a monotonic fallback: if the system clock goes backwards (NTP adjustment, VM time sync), it returns the last known good time instead. This prevents entries from getting incorrect TTL calculations.

## Project Structure

```
src/
├── main.rs              # Entry point, tokio::select! for dual servers
├── lib.rs               # Re-exports for integration tests + benchmarks
├── config.rs            # CLI args + TOML config struct
├── time.rs              # Monotonic time with backward-clock fallback
├── store/
│   ├── mod.rs
│   ├── engine.rs        # Sharded KV engine + vector search + compression
│   ├── shard.rs         # Single shard: papaya HashMap + TTL + compression + capacity
│   ├── memory_type.rs   # MemoryType enum (episodic, semantic, procedural)
│   ├── persistence.rs   # Binary snapshot v2 (with compression flags)
│   └── vector.rs        # HNSW index for semantic search
├── http/
│   ├── mod.rs
│   ├── auth.rs          # AuthStore + axum middleware + namespace extraction
│   ├── ready.rs         # Readiness endpoint for K8s (503 during failover)
│   ├── server.rs        # axum router + handlers (including /search, /share, /metrics)
│   └── models.rs        # JSON request/response types
├── resp/
│   ├── mod.rs
│   ├── error.rs         # RespError enum (thiserror)
│   ├── parser.rs        # RESP2 protocol parser (memchr-accelerated)
│   ├── commands.rs      # Command dispatch → KvEngine
│   ├── session.rs       # ClientSession + process_command_batch
│   ├── server.rs        # Tokio TCP server + connection handling
│   ├── uring_server.rs  # io_uring RESP server (feature-gated)
│   └── tls.rs           # TLS support (rustls or native-tls, feature-gated)
├── llm/
│   ├── mod.rs
│   ├── error.rs         # LlmError enum
│   ├── provider.rs      # LlmProvider trait, LlmRequest, LlmResponse
│   ├── openai.rs        # OpenAI-compatible provider
│   ├── anthropic.rs     # Anthropic Messages API provider
│   └── client.rs        # LlmClient facade (disabled mode, summarize, classify_relevance)
├── replication/
│   ├── mod.rs           # ReplicationState, ReplicationRole, failover config
│   ├── wal.rs           # Write-Ahead Log (circular buffer + serialization)
│   ├── primary.rs       # Primary-side replica tracking + streaming
│   └── replica.rs       # Replica-side sync + stream follower
```
