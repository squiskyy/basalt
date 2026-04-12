# Performance

Basalt is designed for ultra-high performance in AI agent memory workloads. This document covers benchmark results, tuning parameters, and architecture decisions that contribute to performance.

## Benchmark Results

All benchmarks run single-threaded on the core engine with 64 shards and papaya HashMap.

### Core Engine

| Operation | Latency | Throughput (est.) |
|-----------|---------|-------------------|
| **GET** (100K keys) | 323 ns | ~3.1M ops/sec |
| **SET** (16B value) | 1.26 us | ~790K ops/sec |
| **SET** (1KB value) | 2.68 us | ~370K ops/sec |
| **SET** (8KB value) | 4.11 us | ~240K ops/sec |
| **Mixed 90/10** (read-heavy) | 584 ns | ~1.7M ops/sec |

### RESP2 Protocol Parser (memchr SIMD-accelerated)

| Operation | Latency |
|-----------|---------|
| Parse simple string | 88 ns |
| Parse SET command | 190 ns |
| Parse 10-command pipeline | 3.1 us (310 ns/cmd) |
| Parse 100-command pipeline | 29.5 us (295 ns/cmd) |
| Serialize bulk string | 59 ns |

Multi-threaded throughput scales linearly with shards (64 by default) since reads are lock-free.

### Running Benchmarks

```bash
# Core engine benchmarks
cargo bench --bench basalt_bench

# RESP parser benchmarks
cargo bench --bench resp_bench

# With HTML reports
cargo bench --bench basalt_bench -- --save-baseline main
# Reports in target/criterion/
```

## Architecture for Performance

### Lock-Free Reads with papaya

papaya uses a lock-free SwissTable for the `HashMap` in each shard. Reads never block - not on other reads, not on writes. This is the key to the 323ns GET latency.

For AI memory workloads with 50-100:1 read-to-write ratios, papaya significantly outperforms alternatives like dashmap which use fine-grained locks on reads.

### Sharding

Keys are distributed across N shards (default: 64) using ahash with bitwise AND:

```rust
fn shard_index(&self, key: &str) -> usize {
    let mut h = self.hash_state.build_hasher();
    key.hash(&mut h);
    h.finish() as usize & self.shard_mask
}
```

Benefits:
- **No global lock**: Each shard is independent
- **Bitwise AND instead of modulo**: Faster routing (mask vs division)
- **ahash with random seed**: Per-instance randomization prevents hash-flooding attacks
- **Linear scaling**: Throughput scales with the number of shards

### SIMD RESP Parsing

The RESP parser uses `memchr::memchr(b'\r', ...)` to find CRLF delimiters. This dispatches to:
- AVX2/SSE2 on x86_64
- NEON on aarch64

Result: 88ns for simple string parse, 295ns/cmd in 100-command pipelines.

### Pipelining

Multiple RESP commands in a single TCP read are all parsed and their responses batched into one `write_all()`. This eliminates per-command round trips and syscall overhead.

### Buffer Optimization

- 8KB read buffers (fewer syscalls than 4KB)
- Buffer recycling: if a buffer grows past 64KB but is now empty, reset to 8KB
- Track consumed bytes per parse, drain only consumed bytes, keep partial data for next read

### Runtime Compression

Values >= `compression_threshold` bytes (default 1024) are LZ4-compressed in memory using `lz4_flex` (pure Rust, no C dependencies). Decompression is transparent on read.

LZ4 was chosen because:
- Extremely fast decompression (~4GB/s on modern hardware)
- Acceptable compression ratio for text (~50-70% for AI memory content)
- Pure Rust implementation avoids C dependency issues

### Entry Capacity Limits

Each shard has a `max_entries` limit (default: 1,000,000). This prevents OOM under burst writes. The check uses Relaxed atomic ordering - a few extra entries from TOCTOU races are acceptable.

## Tuning Guide

### Shard Count

```bash
basalt --shards 128
```

- Default: 64 (good for up to 64 cores)
- For machines with >64 cores, set `--shards` to the number of cores
- Must be a power of 2 (auto-rounded)
- More shards = less contention, but more memory overhead per shard

### Max Entries

```bash
basalt --max-entries 2000000
```

- Default: 1,000,000 per shard
- Total capacity = max_entries * shard_count (e.g., 64M at default settings)
- Set based on your expected data size + safety margin
- Writes are rejected at capacity; updates to existing keys always succeed

### Compression

```bash
# Enable runtime compression for values >= 512 bytes
basalt --compression-threshold 512

# Disable runtime compression
basalt --compression-threshold 0

# Snapshot compression threshold
basalt --snapshot-compression-threshold 512
```

- Default: 1024 bytes for both
- Lower threshold = more compression, more CPU overhead
- 0 = disabled
- Best for workloads with large text values (AI memory content)
- Small values (< threshold) are never compressed (overhead exceeds savings)

### Sweep Interval

```bash
# Sweep expired entries every 15 seconds
basalt --sweep-interval 15000

# Disable background sweeping
basalt --sweep-interval 0
```

- Default: 30,000ms (30 seconds)
- Lower = cleaner memory, more CPU
- 0 = rely only on lazy eviction (checked on read)
- Episodic-heavy workloads benefit from more frequent sweeping

### Snapshot Interval

```bash
# Snapshot every 30 seconds
basalt --db-path /var/lib/basalt --snapshot-interval 30000

# Manual-only snapshots
basalt --db-path /var/lib/basalt --snapshot-interval 0
```

- Default: 60,000ms (60 seconds) when `db_path` is set
- 0 = only manual snapshots (POST /snapshot or SNAP)
- Snapshots run on a background task and don't block reads/writes
- Snapshot cost scales linearly with entry count

### WAL Size

```bash
basalt --wal-size 50000
```

- Default: 10,000 entries
- Larger WAL = longer replication window before full resync
- Memory cost: ~200 bytes per entry on average
- 50,000 entries ≈ 10MB of WAL memory

### io_uring

```bash
# Build with io_uring support
cargo build --release --features io-uring

# Enable at runtime
basalt --io-uring
```

- Only available on Linux
- Improves RESP server performance by ~1-2us per operation at >10K concurrent connections
- For <10K connections, the tokio epoll server is already fast enough
- The HTTP server always uses tokio (axum), regardless of io_uring

## Monitoring Performance

### INFO Endpoint

```bash
curl http://localhost:7380/info
# or
redis-cli -p 6380 INFO
```

Returns:
- `shard_count`: number of shards
- `role`: primary or replica
- `connected_replicas`: number of connected replicas
- `replication_offset`: current WAL position
- `wal_size`: current WAL entry count

### Log Levels

```bash
# Info level (default)
RUST_LOG=basalt=info basalt

# Debug for performance investigation
RUST_LOG=basalt=debug basalt

# Trace specific modules
RUST_LOG=basalt::store::engine=trace basalt
RUST_LOG=basalt::resp=trace basalt
```

## Performance Anti-Patterns

### Hot Keys

If many agents write to the same key prefix, entries concentrate in fewer shards. Use agent-specific namespaces to distribute load.

### Very Large Values

Values > 8KB have higher SET latency (4.11us vs 1.26us for 16B). If you're storing large documents, consider enabling compression.

### Frequent Full Namespace Scans

`DELETE /store/{namespace}` and `GET /store/{namespace}` without prefix filtering scan all entries in the namespace. For large namespaces, this can be expensive.

### Vector Search on Stale Indexes

Search triggers a full index rebuild when the namespace version has changed since the last build. If you're alternating writes and searches, every search will rebuild. Batch writes before searching to minimize rebuilds.
