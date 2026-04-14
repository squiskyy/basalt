# Basalt Documentation

Ultra-high performance key-value store purpose-built for AI agent memory.

## Guides

- **[Architecture](./architecture.md)** - Internal architecture, data flow, sharding, and design decisions
- **[API Reference](./api-reference.md)** - Complete HTTP REST API and RESP2 protocol documentation
- **[Configuration](./configuration.md)** - All config options (CLI, TOML, environment variables) with examples
- **[Memory Types](./memory-types.md)** - Episodic, semantic, and procedural memory types with TTL behavior
- **[Authentication](./auth.md)** - Bearer token auth, namespace scoping, and token management
- **[Persistence](./persistence.md)** - Binary snapshots, compression, restore, and disk management
- **[Replication](./replication.md)** - Primary-replica async replication, WAL protocol, and failover
- **[Vector Search](./vector-search.md)** - HNSW semantic similarity search with embeddings
- **[LLM Inference](./llm.md)** - Optional LLM integration for summarization, consolidation, and relevance scoring
- **[Performance](./performance.md)** - Benchmark results, tuning guide, and architecture for speed
- **[Deployment](./deployment.md)** - Building, running, systemd, Docker, and production checklist
- **[Development](./development.md)** - Building, testing, code conventions, and contributing

## Quick Links

| What | Where |
|------|-------|
| Build & run | `cargo run --release` |
| HTTP API | `http://localhost:7380` |
| RESP protocol | `redis-cli -p 6380` |
| Config file | `basalt.example.toml` |
| Auth tokens | `tokens.example.txt` |
| Run tests | `cargo test` |
| Run benchmarks | `cargo bench` |

## Project

- **Repository**: [github.com/squiskyy/basalt](https://github.com/squiskyy/basalt)
- **License**: MIT
- **Language**: Rust (edition 2024)
