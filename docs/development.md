# Development

Guide for building, testing, and contributing to Basalt.

## Prerequisites

- Rust 1.94+ (edition 2024)
- A C compiler (for some dependencies)
- Linux recommended (io_uring feature is Linux-only)

## Building

```bash
# Debug build (fast compile, slow runtime)
cargo build

# Release build (slow compile, fast runtime)
cargo build --release

# With io_uring support (Linux only)
cargo build --release --features io-uring
```

The binary is at `target/debug/basalt` or `target/release/basalt`.

## Running Tests

```bash
# All tests
cargo test

# With io_uring feature
cargo test --features io-uring

# Specific test
cargo test test_set_get_basic

# Unit tests only (no integration tests)
cargo test --lib

# Integration tests only
cargo test --test http_integration_test
cargo test --test resp_integration_test
cargo test --test snapshot_test
cargo test --test concurrency_test
cargo test --test vector_test
cargo test --test replication_test
cargo test --test replication_e2e_test

# Show test output
cargo test -- --nocapture

# Run with logging
RUST_LOG=basalt=debug cargo test -- --nocapture
```

**Important**: Always run `cargo test --features io-uring` after changes. The default `cargo test` won't catch feature-gated compilation issues.

## Running Benchmarks

```bash
# Core engine benchmarks
cargo bench --bench basalt_bench

# RESP parser benchmarks
cargo bench --bench resp_bench

# With HTML reports (saved to target/criterion/)
cargo bench
```

Benchmarks use Criterion. Results are in `target/criterion/`.

## Project Structure

```
basalt/
├── src/
│   ├── main.rs              # Entry point, tokio::select! for dual servers
│   ├── lib.rs               # Re-exports for integration tests + benchmarks
│   ├── config.rs            # CLI args (clap) + TOML config
│   ├── metrics.rs           # Prometheus metrics (feature-gated)
│   ├── time.rs              # Monotonic time with backward-clock fallback
│   ├── store/
│   │   ├── mod.rs           # Re-exports
│   │   ├── engine.rs        # Sharded KV engine + vector search + compression
│   │   ├── shard.rs         # Single shard: papaya HashMap + TTL + capacity + LRU
│   │   ├── memory_type.rs   # MemoryType enum
│   │   ├── persistence.rs   # Binary snapshots (v1/v2/v3/v4 with CRC)
│   │   ├── vector.rs        # HNSW index for semantic search
│   │   └── share.rs         # Cross-agent memory sharing (ShareStore)
│   ├── http/
│   │   ├── mod.rs           # Re-exports
│   │   ├── auth.rs          # AuthStore + axum middleware + sharing fallback
│   │   ├── ready.rs         # Readiness endpoint for K8s (503 during failover)
│   │   ├── server.rs        # axum router + handlers (including /share, /metrics)
│   │   └── models.rs        # JSON request/response types (StoreRequest, SearchRequest, etc.)
│   ├── resp/
│   │   ├── mod.rs           # Re-exports
│   │   ├── error.rs         # RespError enum (thiserror)
│   │   ├── parser.rs        # RESP2 parser (memchr-accelerated)
│   │   ├── commands.rs      # Command dispatch -> KvEngine (including VSEARCH, REPLICAOF, AUTH, SHARE)
│   │   ├── session.rs       # ClientSession + process_command_batch (shared by tokio + io_uring)
│   │   ├── server.rs        # Tokio TCP server
│   │   ├── uring_server.rs  # io_uring RESP server (feature-gated)
│   │   └── tls.rs           # TLS support (rustls or native-tls, feature-gated)
│   ├── llm/
│   │   ├── mod.rs           # Re-exports
│   │   ├── error.rs         # LlmError enum (NotConfigured, RequestFailed, etc.)
│   │   ├── provider.rs      # LlmProvider trait, LlmRequest, LlmResponse
│   │   ├── openai.rs        # OpenAI-compatible provider (Ollama, vLLM, LiteLLM)
│   │   ├── anthropic.rs     # Anthropic Messages API provider
│   │   └── client.rs        # LlmClient facade (disabled mode, summarize, classify_relevance)
│   └── replication/
│       ├── mod.rs           # ReplicationState, ReplicationRole, failover config
│       ├── wal.rs           # Write-Ahead Log (binary serialization)
│       ├── primary.rs       # Primary-side replica tracking + streaming
│       └── replica.rs       # Replica-side sync + lease monitoring
├── tests/
│   ├── engine_test.rs       # Unit tests for engine/shard
│   ├── http_integration_test.rs  # HTTP API integration tests
│   ├── resp_integration_test.rs  # RESP protocol integration tests
│   ├── concurrency_test.rs  # Multi-threaded stress tests
│   ├── snapshot_test.rs     # Snapshot round-trip + compression tests
│   ├── vector_test.rs       # Vector search unit tests
│   ├── vector_search_integration_test.rs  # Vector search integration tests
│   ├── replication_test.rs  # WAL + ReplicationState tests
│   ├── replication_e2e_test.rs  # End-to-end replication tests
│   ├── session_test.rs      # ClientSession unit tests (auth, dispatch, REPLICAOF)
│   ├── tls_integration_test.rs  # TLS RESP integration tests (feature-gated)
│   ├── llm_test.rs          # LLM config, client, provider tests
│   ├── io_uring_test.rs     # io_uring feature-gated tests
│   └── common/mod.rs        # Shared test helpers
├── benches/
│   ├── basalt_bench.rs      # Engine benchmarks (set, get, mixed, scan)
│   └── resp_bench.rs        # RESP parser benchmarks
├── basalt.example.toml      # Example configuration file
├── tokens.example.txt       # Example auth tokens file
├── Cargo.toml
└── LICENSE                   # MIT
```

## Code Conventions

### Error Handling

Use `thiserror` for structured error types:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RespError {
    #[error("bind failed: {0}")]
    Bind(#[source] std::io::Error),
    #[error("accept failed: {0}")]
    Accept(#[source] std::io::Error),
}
```

Use explicit `.map_err()` instead of bare `?` for IO errors:

```rust
listener.bind(addr).map_err(RespError::Bind)?;
```

### Feature Flags

Conditional compilation with `#[cfg(feature = "io-uring")]`:

```rust
// In Cargo.toml
[features]
default = []
io-uring = ["dep:io-uring", "dep:slab", "dep:libc"]

// In code
#[cfg(feature = "io-uring")]
pub mod uring_server;
```

**Always test with and without features**:

```bash
cargo test
cargo test --features io-uring
```

Feature-gated function parameters need `#[cfg]` in tests too:

```rust
config.apply_cli_overrides(
    Some("0.0.0.0".to_string()),
    Some(9000u16),
    // ...
    #[cfg(feature = "io-uring")]
    None,  // io_uring - only present when feature is enabled
    // ...
);
```

### Testing Patterns

#### HTTP Integration Tests

```rust
async fn start_default_server() -> (String, tokio::task::JoinHandle<()>, Arc<KvEngine>) {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = app(engine.clone(), auth, None, 1024, None);
    let handle = tokio::spawn(serve(listener, router));
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://127.0.0.1:{port}"), handle, engine)
}
```

Use `reqwest` for HTTP testing. Bind to port 0 for random port allocation.

#### RESP Integration Tests

Build RESP commands manually and use `tokio::net::TcpStream`:

```rust
fn encode_resp_command(name: &str, args: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n${}\r\n{}\r\n", args.len() + 1, name.len(), name).into_bytes();
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n{}\r\n", arg.len(), arg).as_bytes());
    }
    buf
}
```

**Do not use `nc` (netcat)** for RESP testing - it has buffering issues with the RESP protocol.

#### Snapshot Round-Trip Tests

Use `tempfile::tempdir()` for clean directories:

```rust
let dir = tempfile::tempdir().unwrap();
// Create engine -> store data -> snapshot -> new engine -> restore -> verify
```

#### Concurrency Stress Tests

Spawn 100+ tokio tasks with `Arc<KvEngine>` and do concurrent set/get/delete on same keys. Verify no panics, no data corruption, correct final state.

### Adding a New Feature

1. Implement the core logic in the appropriate module (`store/`, `http/`, `resp/`)
2. Add unit tests in the same file (using `#[cfg(test)] mod tests`)
3. Add integration tests in `tests/`
4. Update both HTTP and RESP frontends if the feature is user-facing
5. Update `models.rs` for new HTTP request/response types
6. Update `commands.rs` for new RESP commands
7. Update `persistence.rs` if the feature changes the snapshot format
8. Update `wal.rs` and replication if the feature needs to be replicated
9. Run `cargo test` and `cargo test --features io-uring`
10. Update this documentation

### Commit Messages

Use conventional commit format:

```
feat: add vector search endpoint
fix: handle expired entries in scan_prefix
docs: update API reference for batch endpoints
perf: reduce allocation in RESP parser
refactor: extract auth middleware from server.rs
test: add concurrency stress tests for shard
```

## CI

The project uses GitHub Actions (`.github/workflows/rust.yml`):

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
    - uses: actions/checkout@v4
    - name: Build
      run: cargo build --verbose
    - name: Run tests
      run: cargo test --verbose
```

Consider adding:
- `cargo test --features io-uring` step
- `cargo clippy` step
- `cargo fmt --check` step
- Benchmark comparison on PRs

## Common Pitfalls

### axum Route Conflicts

Two POST handlers on the same path silently conflict. Use distinct paths like `/batch` and `/batch/get`.

### AuthStore Emptiness Trap

When no tokens are configured, `is_authorized()` must return `true` for ALL inputs. A naive "token not found -> false" breaks the no-auth case.

### RESP AUTH Before Dispatch

AUTH must be intercepted in the connection handler loop, NOT in `CommandHandler::handle()`. AUTH modifies per-connection state that the handler doesn't own.

### netcat Can't Test RESP

`nc` has buffering issues with RESP protocol. Use Python sockets or proper Redis client libraries.

### Feature-Gated Parameters in Tests

Methods with `#[cfg(feature = "io-uring")]` conditional parameters need matching `#[cfg]` in test calls. Otherwise tests compile without the feature but fail with it.

### Testing Server Restarts

Don't use `./target/release/basalt &` with `pkill` - background processes can timeout. Use `timeout 5 ./target/release/basalt &` which auto-terminates, then `kill %1; wait %1` for clean shutdown. Add `sleep 2-3` after start and between kill/restart.
