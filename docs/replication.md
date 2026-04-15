# Replication

Basalt supports primary-replica asynchronous replication - high availability built into the single binary, no external orchestrator needed. This allows you to scale reads and provide failover by maintaining copies of your data across multiple nodes.

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
3. Primary sends `+FULLRESYNC <epoch> <seq> <node_id>\r\n` where epoch is the current failover epoch, seq is the current WAL sequence number, and node_id is the primary's unique identifier
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
- **Single replication path**: No chain replication (replica-of-replica)
- **WAL window**: If a replica disconnects for longer than the WAL can hold, it requires a full resync
- **No TLS**: Replication connections are unencrypted (use a VPN or secure network)

## Automatic Failover

Basalt supports automatic failover for Kubernetes deployments. When the primary becomes unreachable, replicas can self-promote without human intervention.

### How It Works

The failover mechanism uses a **lease-based approach** with **epoch fencing**:

1. **Lease heartbeats**: The primary sends `+LEASE <epoch> <offset>` messages to all connected replicas every 5 seconds during the streaming phase
2. **Lease tracking**: Replicas track the last lease timestamp and the primary's epoch
3. **Failover detection**: If `--failover` is enabled and no lease is received within `--failover-timeout` (default 15s), the replica considers the primary dead
4. **Self-promotion**: The replica promotes itself to primary with an incremented epoch, sets `/ready` to 503 during the transition, then sets it back to 200
5. **Split-brain prevention**: Each primary has a monotonically increasing epoch. When a stale primary reconnects and sees a higher epoch, it demotes itself

### Epoch-Based Fencing

Every node has a `current_epoch` (starts at 0). When a replica promotes itself, it increments the epoch to `primary_epoch + 1`. The FULLRESYNC handshake includes the epoch:

```
+FULLRESYNC <epoch> <seq> <node_id>
```

When any node connects to another and sees a higher epoch than its own, it knows it's been replaced and must demote. This prevents split-brain scenarios where two nodes both think they're the primary.

### Protocol Changes

The replication protocol has been extended with two additions:

1. **FULLRESYNC now includes epoch and node_id**: `+FULLRESYNC <epoch> <seq> <node_id>` (backward compatible - old clients ignore extra fields)
2. **LEASE heartbeat**: `+LEASE <epoch> <offset>` sent by the primary every 5 seconds, interleaved with WAL entries

### Configuration

```bash
# Enable automatic failover with default 15s timeout
basalt --failover

# Custom timeout (5 seconds)
basalt --failover --failover-timeout 5000

# Specify peer replicas for reconfiguration
basalt --failover --replica-peers "10.0.1.2:6380,10.0.1.3:6380"
```

Or in TOML:

```toml
[server]
failover = true
failover_timeout_ms = 15000
replica_peers = ["10.0.1.2:6380", "10.0.1.3:6380"]
```

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--failover` | `false` | Enable automatic failover on primary lease expiry |
| `--failover-timeout` | `15000` | Timeout in ms without a lease before considering primary dead |
| `--replica-peers` | none | Comma-separated list of peer `host:port` addresses |

### INFO Replication Output

The `INFO replication` command now includes failover-related fields:

**Primary:**
```
# Replication
role:primary
node_id:a1b2c3d4e5f60789
epoch:0
connected_replicas:2
replication_offset:12345
wal_size:500
failover:enabled
failover_timeout_ms:15000
```

**Replica:**
```
# Replication
role:replica
node_id:f9e8d7c6b5a43210
primary_host:10.0.1.5
primary_port:6380
primary_epoch:0
replication_offset:12340
last_lease_ms:1710000000000
failover:enabled
failover_timeout_ms:15000
```

### Kubernetes Deployment

For K8s, use `--failover` with `--replica-peers` so pods can self-organize:

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: basalt
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: basalt
        image: basalt:latest
        args:
          - --failover
          - --failover-timeout=15000
          - --replica-peers=basalt-0.basalt:6380,basalt-1.basalt:6380,basalt-2.basalt:6380
```

The `/ready` endpoint returns 503 during failover transitions, which K8s uses for readiness probes.

### Failover Sequence

1. Primary sends `+LEASE <epoch> <offset>` every 5 seconds to connected replicas
2. Primary becomes unreachable (crash, network partition, etc.)
3. Replica's failover monitor detects lease expiry after `--failover-timeout`
4. Replica sets `/ready` to 503 ("failover_in_progress")
5. Replica promotes itself to primary: `role = Primary`, `epoch = primary_epoch + 1`
6. Replica sets `/ready` to 200 (ready to accept writes)
7. Other replicas reconnect (via peer list or K8s service discovery)
8. Old primary (if it recovers) connects and sees higher epoch -> demotes itself

## Configuration Reference

| Setting | Default | Description |
|---------|---------|-------------|
| `--wal-size` | `10000` | Maximum WAL entries before oldest are evicted |
| `--db-path` | none | Required for replication snapshots |
| `--failover` | `false` | Enable automatic failover on primary lease expiry |
| `--failover-timeout` | `15000` | Timeout in ms without lease before considering primary dead |
| `--replica-peers` | none | Comma-separated list of peer `host:port` addresses |

Increase `--wal-size` if:
- Replicas have high network latency
- You want a longer grace period before full resyncs are needed
- Your write volume is very high (more entries = larger window)
