# Vector Search

Basalt includes built-in semantic similarity search using HNSW (Hierarchical Navigable Small World) approximate nearest neighbor indexes - no separate vector database required. This enables finding memories by embedding vector proximity, which is essential for AI agents that work with embeddings.

## Overview

- Per-namespace HNSW indexes using the `instant-distance` crate (pure Rust, no C dependencies)
- Embeddings are stored alongside entries as `Option<Vec<f32>>`
- Indexes are lazily rebuilt when stale (tracked via version counters)
- Cosine distance metric for similarity measurement
- HTTP and RESP interfaces for search

## Storing Entries with Embeddings

### HTTP

```bash
curl -X POST http://localhost:7380/store/agent-42 \
  -H 'Content-Type: application/json' \
  -d '{
    "key": "fact:gravity",
    "value": "gravity pulls objects toward each other",
    "type": "semantic",
    "embedding": [0.1, 0.2, 0.3, 0.4, 0.5]
  }'
```

The `embedding` field is optional. Entries without embeddings are included in prefix scans but excluded from vector search.

### RESP

Use `MSETT` for entries with embeddings (stored via `set_with_embedding` on the engine). For direct embedding search via RESP, use the `VSEARCH` command.

## Searching

### HTTP

```
POST /store/{namespace}/search
```

Request body:

```json
{
  "embedding": [0.12, 0.25, 0.33, 0.41, 0.48],
  "top_k": 10
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `embedding` | Yes | Float array to search against |
| `top_k` | No | Number of results (default: 10) |

Response:

```json
{
  "results": [
    {
      "key": "fact:gravity",
      "value": "gravity pulls objects toward each other",
      "type": "semantic",
      "distance": 0.0032
    },
    {
      "key": "fact:physics",
      "value": "F = ma",
      "type": "semantic",
      "distance": 0.0154
    }
  ]
}
```

### RESP

```
VSEARCH agent-42 [0.12,0.25,0.33,0.41,0.48] COUNT 5
```

- `namespace` - namespace to search within
- `embedding` - JSON array of floats
- `COUNT N` - optional, number of results (default: 10)

Response: Array of `[key, value, distance]` triples.

## How It Works

### Embedding Storage

Embeddings are stored in the `Entry` struct alongside the value:

```rust
pub struct Entry {
    pub value: Vec<u8>,
    pub compressed: bool,
    pub expires_at: Option<u64>,
    pub memory_type: MemoryType,
    pub embedding: Option<Vec<f32>>,
}
```

Not all entries have embeddings. Regular key-value pairs have `embedding: None`.

### HNSW Index

Each namespace gets its own HNSW index:

```rust
pub struct HnswIndex {
    index: instant_distance::Hnsw<EmbeddingPoint>,
    version: u64,
    keys: Vec<String>,
}
```

The index is stored at the engine level in a `Mutex<HashMap<String, HnswIndex>>`.

### Lazy Rebuild

Indexes are not updated on every write. Instead, each namespace has a version counter (AtomicU64). Any `set` or `delete` in that namespace increments the counter. On search, the index version is compared to the namespace version:

- If they match: use the existing index (fast path)
- If they don't match: rebuild the index from all entries with embeddings in that namespace, then search

This means:
- Read-heavy workloads (typical for AI memory) rarely trigger rebuilds
- Write-heavy workloads may see occasional search latency spikes
- The rebuild is O(n) where n = number of entries with embeddings in the namespace

### Cosine Distance

The `EmbeddingPoint` type implements `instant_distance::Point` with cosine distance:

```rust
pub struct EmbeddingPoint(Vec<f32>);

impl Point for EmbeddingPoint {
    fn distance(&self, other: &Self) -> f32 {
        cosine_distance(&self.0, &other.0)
    }
}
```

Cosine distance is `1 - cosine_similarity`, where:
- 0 = identical direction (most similar)
- 1 = orthogonal (unrelated)
- 2 = opposite direction (most dissimilar)

## Performance Considerations

### Index Rebuild Cost

Rebuilding an HNSW index is O(n * log(n) * M) where M is the HNSW connectivity parameter. For a namespace with 10,000 embeddings, a rebuild takes a few hundred milliseconds. For 100,000 embeddings, it may take a few seconds.

Mitigation strategies:
- Use smaller namespaces (each namespace has its own index)
- Batch writes before searching (avoid the read-modify-rebuild cycle)
- The version check avoids unnecessary rebuilds

### Memory Usage

Each embedding vector adds `4 * dimensions` bytes per entry (f32 per dimension). For 768-dimensional embeddings (common for sentence transformers):
- Per entry: ~3 KB
- 100,000 entries: ~300 MB
- Plus HNSW index overhead (~2-3x the raw embedding data)

### Search Latency

HNSW search is O(log(n)) for approximate nearest neighbors. Typical latency:
- 10,000 entries: < 1ms
- 100,000 entries: < 5ms
- 1,000,000 entries: < 20ms

Plus the one-time rebuild cost if the index is stale.

## Replication

Embeddings are included in replication:
- WAL entries carry the embedding vector
- Snapshot entries include embeddings
- Vector indexes on replicas are rebuilt lazily (same as on the primary)

## Persistence

Embeddings are preserved in snapshots. Entries with embeddings include the embedding data in the snapshot format (with an embedding flag byte indicating presence).
