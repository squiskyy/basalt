# Persistence

Basalt supports optional disk persistence via binary snapshots - no external database required. When `db_path` is configured, the engine writes snapshots to disk and restores them on startup.

## Overview

- Snapshots are written to a configurable directory
- Format: custom binary with magic header + version + length-prefixed entries
- Writes are atomic (write to `.tmp`, then `fs::rename`)
- Old snapshots are auto-pruned (keeps last 3 by default)
- On startup, the latest snapshot is loaded automatically
- Expired entries are skipped during restore

## Configuration

```toml
[server]
db_path = "/var/lib/basalt"
snapshot_interval_ms = 60000
snapshot_compression_threshold = 1024
```

Or via CLI:

```bash
basalt --db-path /var/lib/basalt --snapshot-interval 30000 --snapshot-compression-threshold 512
```

| Setting | Default | Description |
|---------|---------|-------------|
| `db_path` | none (disabled) | Directory for snapshot files |
| `snapshot_interval_ms` | `60000` (60s) | Auto-snapshot interval; `0` = manual only |
| `snapshot_compression_threshold` | `1024` | Min value size (bytes) for LZ4 compression; `0` = disable |

Without `db_path`, persistence is completely disabled. `POST /snapshot` returns `412` and `SNAP` returns an error.

## Snapshot Format

### v1 Format

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

### v2 Format (with compression)

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

- The `flags` byte enables future extensions (bits 1-7 are reserved)
- Only compresses if LZ4 actually reduces the size (incompressible data stays uncompressed)
- v2 reader can read v1 snapshots (backward compatible - no flags byte when version=1)

### v3 Format (with embedding vectors)

Same header but VERSION = 3. Adds an embedding flag and optional embedding data per entry:

```
Per entry:
  key_len:        u32 LE
  key:            [u8; key_len]
  flags:          u8                  (bit 0 = compressed)
  val_len:        u32 LE
  value:          [u8; val_len]       (LZ4-compressed if flag bit 0 set)
  mem_type:       u8
  embedding_flag: u8                  (0 = no embedding, 1 = has embedding)
  expires_at:     u64 LE
  If embedding_flag == 1:
    dim:          u32 LE              (number of f32 dimensions)
    embedding:    [u8; dim * 4]       (each f32 as 4 LE bytes)
```

- v3 reader can read v1 and v2 snapshots (backward compatible)

### v4 Format (with CRC32 checksums)

Current format (VERSION = 4). Adds per-entry and footer CRC32 checksums for corruption detection:

```
MAGIC:        b"BASALT\x00" (7 bytes)
VERSION:      u8 (1 byte, value = 4)
COUNT:        u64 LE (8 bytes)

Per entry (same fields as v3, plus entry_crc):
  key_len:        u32 LE
  key:            [u8; key_len]
  flags:          u8
  val_len:        u32 LE
  value:          [u8; val_len]
  mem_type:       u8
  embedding_flag: u8
  expires_at:     u64 LE
  If embedding_flag == 1:
    dim:          u32 LE
    embedding:    [u8; dim * 4]
  entry_crc:      u32 LE              (CRC32 of all entry bytes from key_len through embedding)

Footer:
  footer_crc:     u32 LE              (CRC32 of all bytes from MAGIC through last entry_crc)
```

**Why checksums?** The `.tmp` + `rename` atomic write pattern protects against partial writes from the snapshot process, but does not protect against:
- Filesystem corruption
- Bit rot on disk
- Accidental manual editing of snapshot files
- Disk full during write (rename succeeds, data truncated)
- Crash between write and fsync

**Recovery behavior:**
- Per-entry CRC mismatch: the entry is skipped with a warning, restore continues with remaining entries
- Footer CRC mismatch: a warning is logged, but entries that passed their individual CRC checks are still loaded
- v1-v3 snapshots: still readable with a logged warning suggesting re-save to upgrade format
- Re-saving a snapshot (triggering a new snapshot write) upgrades it to v4 format

**Performance:** CRC32 via `crc32fast` adds negligible overhead - ~4 bytes per entry + 4 bytes footer, with hardware-accelerated CRC computation on modern CPUs.

### Memory Type Encoding

| Value | Type |
|-------|------|
| 0 | Episodic |
| 1 | Semantic |
| 2 | Procedural |

### TTL Encoding

`expires_at` is a UNIX timestamp in milliseconds:
- `0` = no expiry (semantic/procedural, or episodic without TTL)
- `> 0` = absolute expiry time in ms since epoch

## Snapshot Triggers

### Automatic (background loop)

When `snapshot_interval_ms > 0`, a background tokio task runs a snapshot loop:

```
loop {
    select! {
        _ = sleep(interval) => {
            snapshot(&db_path, &engine, keep=3)
        }
        _ = shutdown.changed() => {
            // Final snapshot on graceful shutdown
            snapshot(&db_path, &engine, keep=3)
            break
        }
    }
}
```

The shutdown signal is sent via `tokio::sync::watch` when the process receives SIGTERM/SIGINT.

### Manual

- **HTTP**: `POST /snapshot` - returns `{"ok":true,"path":"...","entries":N}`
- **RESP**: `SNAP` - returns bulk string with path or `-ERR no db_path configured`

Both trigger an immediate snapshot regardless of the auto-snapshot interval.

### Graceful Shutdown

When the process shuts down (SIGTERM/SIGINT), the snapshot loop receives a shutdown signal and performs one final snapshot before exiting. This ensures the latest state is persisted.

## Snapshot Lifecycle

1. **Write**: Serialize all live entries via `engine.scan_prefix("")`
2. **Atomic write**: Write to `<db_path>/.tmp` then `fs::rename()` to final path
3. **Naming**: `snapshot-<unix_timestamp_millis>.bin`
4. **Auto-prune**: After writing, delete the oldest snapshots keeping only the last 3
5. **Restore**: On startup, find the latest snapshot file, deserialize, insert via `set_force()` (bypasses capacity checks), skip expired entries

## Entry Collection

Snapshots iterate all live entries using `scan_prefix("")`:

```rust
pub fn collect_entries(engine: &KvEngine) -> Vec<SnapshotEntry> {
    engine.scan_prefix("").into_iter().map(|(key, value, meta)| {
        SnapshotEntry {
            key, value,
            memory_type: meta.memory_type,
            expires_at: meta.ttl_remaining_ms.map(|ttl| now_ms() + ttl),
        }
    }).collect()
}
```

TTL is converted from remaining ms to absolute `expires_at` timestamp at snapshot time. On restore, entries where `expires_at < now` are skipped (already expired).

## Compression

Both snapshot and runtime compression use `lz4_flex` (pure Rust, no C dependencies):

### Snapshot Compression

When `snapshot_compression_threshold > 0`, values larger than the threshold are LZ4-compressed in the snapshot file:

- Compressed values have the `flags` bit 0 set in the v2 format
- Incompressible data (where LZ4 doesn't reduce size) is stored uncompressed
- Set `snapshot_compression_threshold = 0` to disable

### Runtime Compression

When `compression_threshold > 0`, values larger than the threshold are LZ4-compressed in memory:

- Compression happens on `set()` and `set_force()`
- Decompression is transparent on `get()`, `scan_prefix()`, `get_with_meta()`
- The `Entry.compressed` flag tracks whether the stored value is compressed
- Set `compression_threshold = 0` to disable

### When to Use Compression

Compression is most beneficial when:
- Values contain repetitive text (common for AI memory content)
- You have many large values (>1KB)
- Memory usage is a concern

Compression has overhead for small values. The default 1024-byte threshold avoids compressing small values where the overhead exceeds the savings.

## Snapshot File Management

### Finding the Latest Snapshot

`find_latest_snapshot()` iterates the `db_path` directory and picks the file with the highest timestamp in its name. Since files are named `snapshot-<timestamp_millis>.bin`, lexicographic sorting works.

### Auto-Pruning

After each snapshot write, Basalt deletes the oldest snapshot files, keeping only the last 3 (by timestamp). This prevents unbounded disk usage.

### File Permissions

Snapshots are written with default OS file permissions. The `db_path` directory must exist and be writable before starting Basalt.

### Disk Space Estimate

Each entry in a snapshot takes roughly:
- 4 bytes (key_len) + key bytes + 1 byte (flags) + 4 bytes (val_len) + value bytes + 1 byte (mem_type) + 1 byte (embedding_flag) + 8 bytes (expires_at) + 4 bytes (entry_crc)
- Plus 16 bytes header + 4 bytes footer_crc for the whole file

For 1 million entries averaging 256 bytes per value:
- Uncompressed: ~270 MB per snapshot
- With LZ4 compression: typically 50-70% smaller for text data
- 3 snapshots: ~810 MB uncompressed, ~270-540 MB compressed

## Startup Restore

1. If `db_path` is configured, find the latest snapshot file
2. Read and validate the magic header and version
3. Deserialize each entry:
   - If version 4, verify per-entry CRC32 (skip corrupt entries with warning, continue with rest)
   - If version 4, verify footer CRC32 after all entries (warn on mismatch)
   - If version 3, read embedding vectors (no checksums - warn about missing checksums)
   - If version 2 and flags bit 0 is set, decompress the value (no checksums - warn)
   - If version 1, no flags byte (backward compatible - warn)
4. Insert each valid entry via `engine.set_force()` which bypasses capacity limits
5. Skip entries where `expires_at < now` (already expired)
6. Log the number of restored entries, plus any corrupt entries skipped

`set_force()` is used instead of `set()` so that snapshots always restore fully regardless of the current `max_entries` setting.

v1-v3 snapshots are fully readable but will log a warning recommending re-save to upgrade to v4 format. Triggering a new snapshot (via auto-snapshot interval, manual `POST /snapshot`, or `SNAP` command) writes a v4 file, effectively upgrading the format.

## Monitoring

Check snapshot status via the `INFO` command or endpoint:

```bash
# HTTP
curl http://localhost:7380/info

# RESP
redis-cli -p 6380 INFO
```

The logs show snapshot events at info level:
```
basalt=info basalt --db-path /var/lib/basalt
# Logs: "auto-snapshot saved: /var/lib/basalt/snapshot-1712951000000.bin"
# Logs: "restored 42857 entries from snapshot"
```
