# Replication

Basalt supports primary-replica asynchronous replication. This allows you to scale reads and provide high availability by maintaining copies of your data across multiple nodes.

## Overview

```
┌─────────────┐         ┌─────────────┐
│   Primary    │────────►│   Replica 1  │
│  (port 6380) │────┐    │  (port 6380) │
│              │    │    └─────────────┘
└─────────────┘    │
                   │    ┌─────────────┐
                   └───►│   Replica 2  │
                        │  (port 6380) │
                        └─────────────┘
```

- Replication uses the existing RESP protocol (port 6380)
- Replication is asynchronous: the primary does not wait for replicas to acknowledge writes
- Replicas can serve reads locally after applying the replicated data
- A replica can be promoted to primary with `REPLICAOF NO ONE`

## Write-Ahead Log (WAL)

The WAL is a circular buffer of recent write operations, enabling incremental replication.

### Structure

```
Wal {
    entries: VecDeque<(u64, WalEntry)>,  // circular buffer
    max_size: usize,                       // configurable (default 10000)
    notify: Arc<Notify>,                   // wake streaming replicas
}
```

### WAL Entry

```rust
pub struct WalEntry {
    pub op: WalOp,             // Set, Delete, or DeletePrefix
    pub key: Vec<u8>,          // full internal key (namespace:key)
    pub value: Vec<u8>,        // value bytes
    pub mem_type: MemoryType,  // episodic, semantic, or procedural
    pub ttl_ms: u64,           // 0 = no expiry, >0 = expire in N ms
    pub timestamp_ms: u64,     // when the write occurred
    pub embedding: Option<Vec<f32>>,  // optional vector for search
}
```

### Wire Format

Each WAL entry is serialized for replication streaming:

```
seq:          u64 LE   (sequence number)
op:           u8       (0=Set, 1=Delete, 2=DeletePrefix)
timestamp:    u64 LE
key_len:      u32 LE
key:          [u8; key_len]
val_len:      u32 LE
value:        [u8; val_len]
mem_type:     u8       (0=episodic, 1=semantic, 2=procedural)
ttl_ms:       u64 LE   (0 = no expiry)
embedding_flag: u8     (0 = no embedding, 1 = embedding present)
[if embedding present]:
  dim:        u32 LE
  data:       dim * f32 LE bytes
```

### Behavior

- Oldest entries are evicted when the buffer exceeds `max_size`
- Each entry gets a monotonically increasing sequence number
- `entries_from(seq)` returns all entries with sequence >= seq (uses binary search)
- The `Notify` channel wakes streaming replicas when new entries are appended

### Configuration

```bash
basalt --wal-size 50000
```

Or in TOML:

```toml
[server]
wal_size = 50000
```

Default: 10,000 entries. Increase this if replicas have high latency or you want a longer replication window.

## Replication Protocol

### Initial Handshake

1. Replica connects to primary's RESP port
2. Replica sends `REPLICAOF <host> <port>` (this is handled at the connection level, not in CommandHandler)
3. Primary sends `+FULLRESYNC <seq>\r\n` where seq is the current WAL sequence number
4. Primary sends the full snapshot as RESP bulk arrays (entry count as RESP Integer, then each entry as RESP Array)
5. Primary sends `+STREAM\r\n` to indicate the snapshot is complete and streaming begins

### Snapshot Transfer

The primary serializes all live entries via `scan_prefix_with_embeddings("")` and sends them as RESP arrays:

```
*6\r\n
$3\r\nSET\r\n
$14\r\nagent-42:obs:1\r\n
$11\r\nsaw a red car\r\n
$9\r\nepisodic\r\n
$7\r\n3600000\r\n
$0\r\n\r\n     (embedding: empty = none)
```

Each entry includes: SET command, key, value, memory type, TTL, and embedding (empty string if none).

The replica applies all snapshot entries via `set_force_with_embedding()` which bypasses capacity limits.

### Streaming WAL Entries

After the snapshot, the primary streams new WAL entries as they arrive:

1. Primary appends to WAL on each write (set, delete, delete_prefix)
2. `Notify` channel wakes the streaming task
3. Primary serializes the WalEntry and sends it as a RESP BulkString
4. Replica receives, deserializes, and applies the entry to its local engine
5. Replica updates its replication offset

### Backpressure

Each replica connection has a 64KB pending buffer limit. If the accumulated size of pending WAL entries exceeds this, the primary flushes the write buffer before sending more. This prevents overwhelming slow replicas.

### Replica Falling Behind

If a replica's replication offset falls behind the WAL's oldest entry (meaning the needed entries have been evicted from the circular buffer), the primary detects this and triggers a full resync:

```
WARNING: replica fell behind: replica offset 950 is behind WAL oldest seq 1001.
Triggering full resync.
```

The replica will need to reconnect and perform a fresh FULLRESYNC.

### Graceful Shutdown

Replicas monitor a shutdown channel. When the primary shuts down, the replica receives a signal and disconnects cleanly. The `REPLICAOF NO ONE` command also cleanly disconnects from the primary.

## Replication State

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

### Methods

- `record_set()`, `record_set_with_embedding()` - append SET to WAL
- `record_delete()` - append DELETE to WAL
- `record_delete_prefix()` - append DELETE_PREFIX to WAL
- `wal()` - access the WAL for streaming
- `inc_connected_replicas()` / `dec_connected_replicas()` - track replica count
- `replication_offset()` / `set_replication_offset()` - track replication position

## Using Replication

### Set Up a Primary

```bash
# Start the primary with a WAL size
basalt --db-path /var/lib/basalt --wal-size 50000
```

### Set Up a Replica

```bash
# Start a replica pointing at the primary
basalt --db-path /var/lib/basalt-replica
```

Then connect to the replica's RESP port and issue:

```
REPLICAOF 10.0.1.5 6380
```

The replica will:
1. Connect to the primary
2. Clear its local data
3. Receive and apply the full snapshot
4. Begin streaming WAL entries

### Check Status

```
INFO
```

Response includes:
- `role`: `primary` or `replica`
- `connected_replicas`: number of connected replicas (primary only)
- `replication_offset`: current WAL position
- `wal_size`: current WAL entry count

### Promote a Replica

```
REPLICAOF NO ONE
```

The replica disconnects from the primary and becomes a primary itself. Note: it does not automatically reconfigure other replicas to follow it. You would need to issue `REPLICAOF` commands on other replicas to point them at the new primary.

## Limitations

- **Asynchronous only**: Writes are not acknowledged by replicas before responding to the client
- **No automatic failover**: If the primary dies, you must manually promote a replica
- **Single replication path**: No chain replication (replica-of-replica)
- **WAL window**: If a replica disconnects for longer than the WAL can hold, it requires a full resync
- **No TLS**: Replication connections are unencrypted (use a VPN or secure network)

## Configuration Reference

| Setting | Default | Description |
|---------|---------|-------------|
| `--wal-size` | `10000` | Maximum WAL entries before oldest are evicted |
| `--db-path` | none | Required for replication snapshots |

Increase `--wal-size` if:
- Replicas have high network latency
- You want a longer grace period before full resyncs are needed
- Your write volume is very high (more entries = larger window)
