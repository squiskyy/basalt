# Configuration

Basalt can be configured via CLI flags, a TOML config file, or both. CLI flags always override config file values.

## Quick Start

```bash
# Minimal - all defaults
basalt

# With config file
basalt --config /etc/basalt/basalt.toml

# Override specific settings
basalt --http-port 8080 --shards 128 --auth "bsk-admin:*"
```

## Config File

Create a TOML file and pass it with `--config`:

```bash
basalt --config /etc/basalt/basalt.toml
```

Full example (see `basalt.example.toml`):

```toml
[server]
# HTTP REST API bind address
http_host = "127.0.0.1"
http_port = 7380

# RESP2 (Redis-compatible) bind address
resp_host = "127.0.0.1"
resp_port = 6380

# Number of shards (each is an independent concurrent HashMap)
# Must be a power of 2. Default: 64
shard_count = 64

# Directory for persistence snapshots
# When set, snapshots are written to this directory.
# On startup, the latest snapshot is loaded automatically.
db_path = "/var/lib/basalt"

# How often to auto-snapshot (milliseconds). Default: 60000 (60s)
# Set to 0 to disable auto-snapshots (manual only via POST /snapshot or SNAP).
snapshot_interval_ms = 60000

# How often to sweep expired entries (milliseconds). Default: 30000 (30s)
# Set to 0 to disable background sweeping.
sweep_interval_ms = 30000

# Maximum entries per shard. Default: 1000000
# When a shard is at capacity, new keys are rejected (507 on HTTP, ERR on RESP).
# Updates to existing keys always succeed.
max_entries = 1000000

# Minimum value size (bytes) to LZ4-compress in snapshots. Default: 1024
# Set to 0 to disable snapshot compression.
snapshot_compression_threshold = 1024

# Minimum value size (bytes) to LZ4-compress values in memory at runtime. Default: 1024
# Set to 0 to disable runtime compression.
compression_threshold = 1024

# WAL size for replication (number of entries). Default: 10000
wal_size = 10000

[auth]
# Path to auth tokens file (one token per line)
# Format: <token> <namespace1> [namespace2 ...]
# Use * for wildcard (all namespaces)
# tokens_file = "/etc/basalt/tokens.txt"
```

## CLI Flags

All server settings are available as CLI flags. These override config file values.

### Server

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--config` | path | - | Path to TOML config file |
| `--http-host` | string | `127.0.0.1` | HTTP bind address |
| `--http-port` | u16 | `7380` | HTTP port |
| `--resp-host` | string | `127.0.0.1` | RESP bind address |
| `--resp-port` | u16 | `6380` | RESP port |
| `--shards` | usize | `64` | Number of shards (rounded to next power of 2) |

### Persistence

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--db-path` | string | none | Directory for snapshots |
| `--snapshot-interval` | u64 | `60000` | Auto-snapshot interval in ms (0 = disabled) |
| `--snapshot-compression-threshold` | usize | `1024` | Min bytes for LZ4 compression in snapshots (0 = disabled) |

### Memory Management

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--max-entries` | usize | `1000000` | Max entries per shard |
| `--sweep-interval` | u64 | `30000` | Expired entry sweep interval in ms (0 = disabled) |
| `--compression-threshold` | usize | `1024` | Min bytes for LZ4 runtime compression (0 = disabled) |

### Replication

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--wal-size` | usize | `10000` | WAL buffer size (number of entries) |
| `--failover` | bool | `false` | Enable automatic failover on primary lease expiry |
| `--failover-timeout` | u64 | `15000` | Timeout in ms without lease before considering primary dead |
| `--replica-peers` | string | - | Comma-separated list of peer `host:port` addresses |

### Authentication

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--auth` | string (repeatable) | - | Auth token spec: `"token:ns1,ns2"` or `"token:*"` |
| `--auth-file` | path | - | Path to auth tokens file |

### io_uring (Linux only, requires feature flag)

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--io-uring` | bool | `false` | Use io_uring for RESP server |

Only available when compiled with `--features io-uring`.

## Auth Tokens File

A plain text file with one token per line:

```
# Comments start with #
# Format: <token> <namespace1> [namespace2 ...]
# Use * for wildcard access

bsk-admin-secret *
bsk-agent1-abc123 agent-1 shared
bsk-agent2-def456 agent-2
```

- Whitespace-delimited (not colon-delimited like CLI `--auth` flags)
- Lines starting with `#` are comments
- Blank lines are ignored
- CLI `--auth` flags override file entries with the same token value

See `tokens.example.txt` for a template.

## CLI vs File Token Formats

The two formats use different delimiters:

**CLI**: `--auth "bsk-admin:*"`
- Colon between token and namespaces
- Commas between multiple namespaces

**File**: `bsk-admin *`
- Whitespace between token and namespaces
- Spaces between multiple namespaces

This is intentional: CLI args are single strings that need explicit separators, while files have line structure where whitespace splitting is natural.

## Override Priority

1. CLI flags (highest priority)
2. Config file values
3. Built-in defaults (lowest priority)

Each CLI flag uses `Option<>` internally so the system can distinguish "user didn't pass this flag" from "user passed the default value."

## Production Example

```toml
[server]
http_host = "0.0.0.0"
http_port = 7380
resp_host = "0.0.0.0"
resp_port = 6380
shard_count = 128
db_path = "/var/lib/basalt"
snapshot_interval_ms = 30000
sweep_interval_ms = 15000
max_entries = 500000
compression_threshold = 512
snapshot_compression_threshold = 512
wal_size = 50000

[auth]
tokens_file = "/etc/basalt/tokens.txt"
```

```bash
# Override specific values from the config file
basalt --config /etc/basalt/basalt.toml --shards 256 --max-entries 2000000
```

## Environment Variables

Basalt uses `RUST_LOG` for log filtering (via `tracing_subscriber`):

```bash
# Show all basalt logs at info level
RUST_LOG=basalt=info basalt

# Debug level for everything
RUST_LOG=debug basalt

# Debug only the RESP server
RUST_LOG=basalt::resp=debug basalt

# Trace the engine internals
RUST_LOG=basalt::store=trace basalt
```

Default log level: `basalt=info`
