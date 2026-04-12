/// Vector search module for semantic memory embeddings.
///
/// Provides HNSW-based approximate nearest neighbor search using `instant-distance`.
/// The index is lazily built: it's rebuilt on demand when a search is requested
/// and the namespace version has changed since the last build.
///
/// The index is per-namespace, stored at the engine level, keyed by namespace name.
/// Version counters are per-namespace (tracked in KvEngine) to avoid unnecessary
/// rebuilds of unrelated namespace indexes.
use std::collections::HashMap;

use instant_distance::{HnswMap, Point, Search};

/// A wrapper around a float vector that implements the `Point` trait for instant-distance.
/// Uses cosine distance for semantic similarity.
#[derive(Clone)]
pub struct EmbeddingPoint {
    values: Vec<f32>,
}

impl EmbeddingPoint {
    pub fn new(values: Vec<f32>) -> Self {
        Self { values }
    }

    /// Compute the norm (L2 magnitude) of this vector.
    fn norm(&self) -> f32 {
        self.values.iter().map(|v| v * v).sum::<f32>().sqrt()
    }
}

impl Point for EmbeddingPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Cosine distance = 1 - cosine_similarity
        // cosine_similarity = (a . b) / (||a|| * ||b||)
        let dot: f32 = self
            .values
            .iter()
            .zip(other.values.iter())
            .map(|(a, b)| a * b)
            .sum();

        let norm_a = self.norm();
        let norm_b = other.norm();

        if norm_a == 0.0 || norm_b == 0.0 {
            // If either vector is zero, return max distance
            return 1.0;
        }

        let similarity = dot / (norm_a * norm_b);
        // Clamp to [-1, 1] to handle floating point errors
        let similarity = similarity.clamp(-1.0, 1.0);
        1.0 - similarity
    }
}

/// A search result entry with key, distance, and value.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub key: String,
    /// Cosine distance (0 = identical, 2 = opposite). Lower is more similar.
    pub distance: f32,
    /// The stored value associated with this key.
    pub value: Vec<u8>,
}

/// Per-namespace HNSW index wrapper.
///
/// Tracks a version counter that is compared against the namespace version
/// to determine if the index needs rebuilding. Also caches the values map
/// so we avoid rebuilding it on every search when the index is fresh.
pub struct HnswIndex {
    /// The HNSW map: points -> keys as values.
    index: Option<HnswMap<EmbeddingPoint, String>>,
    /// The version of the namespace data when the index was last built.
    index_version: u64,
    /// Cached values map for returning search results. Rebuilt alongside the index.
    cached_values: HashMap<String, Vec<u8>>,
}

impl HnswIndex {
    /// Create a new empty HNSW index.
    pub fn new() -> Self {
        HnswIndex {
            index: None,
            index_version: 0,
            cached_values: HashMap::new(),
        }
    }

    /// Rebuild the HNSW index from a list of (key, embedding, value) tuples.
    ///
    /// Entries without embeddings are skipped. Also caches the values map
    /// for fast search result lookups without re-scanning the namespace.
    pub fn rebuild(&mut self, entries: &[(String, Vec<f32>, Vec<u8>)], namespace_version: u64) {
        let points: Vec<EmbeddingPoint> = entries
            .iter()
            .map(|(_, emb, _)| EmbeddingPoint::new(emb.clone()))
            .collect();
        let values: Vec<String> = entries.iter().map(|(k, _, _)| k.clone()).collect();

        // Cache the values map for search results
        self.cached_values = entries
            .iter()
            .map(|(k, _, v)| (k.clone(), v.clone()))
            .collect();

        if points.is_empty() {
            self.index = None;
            self.index_version = namespace_version;
            return;
        }

        let map: HnswMap<EmbeddingPoint, String> =
            instant_distance::Hnsw::<EmbeddingPoint>::builder()
                .ef_construction(150)
                .ef_search(100)
                .build(points, values);

        self.index = Some(map);
        self.index_version = namespace_version;
    }

    /// Check if the index needs to be rebuilt for the given namespace version.
    pub fn is_stale(&self, namespace_version: u64) -> bool {
        self.index_version != namespace_version
    }

    /// Search for the top-K nearest neighbors of the given embedding.
    ///
    /// Returns results sorted by distance (closest first).
    /// Uses the cached values map from the last rebuild.
    pub fn search(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Vec<VectorSearchResult> {
        let index = match &self.index {
            Some(idx) => idx,
            None => return Vec::new(),
        };

        let query = EmbeddingPoint::new(embedding.to_vec());
        let mut search = Search::default();

        let results: Vec<VectorSearchResult> = index
            .search(&query, &mut search)
            .take(top_k)
            .filter_map(|item| {
                let key = item.value.clone();
                let value = self.cached_values.get(&key)?;
                Some(VectorSearchResult {
                    key,
                    distance: item.distance,
                    value: value.clone(),
                })
            })
            .collect();

        results
    }
}

impl Default for HnswIndex {
    fn default() -> Self {
        Self::new()
    }
}

// Per-namespace version counters are now tracked in KvEngine
// (see src/store/engine.rs) instead of a single global counter.
// This avoids unnecessary rebuilds of unrelated namespace indexes.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_point_distance_identical() {
        let a = EmbeddingPoint::new(vec![1.0, 0.0, 0.0]);
        let b = EmbeddingPoint::new(vec![1.0, 0.0, 0.0]);
        let dist = a.distance(&b);
        assert!(
            (dist - 0.0).abs() < 1e-5,
            "identical vectors should have 0 distance, got {dist}"
        );
    }

    #[test]
    fn test_embedding_point_distance_opposite() {
        let a = EmbeddingPoint::new(vec![1.0, 0.0, 0.0]);
        let b = EmbeddingPoint::new(vec![-1.0, 0.0, 0.0]);
        let dist = a.distance(&b);
        assert!(
            (dist - 2.0).abs() < 1e-5,
            "opposite vectors should have distance 2.0, got {dist}"
        );
    }

    #[test]
    fn test_embedding_point_distance_orthogonal() {
        let a = EmbeddingPoint::new(vec![1.0, 0.0]);
        let b = EmbeddingPoint::new(vec![0.0, 1.0]);
        let dist = a.distance(&b);
        assert!(
            (dist - 1.0).abs() < 1e-5,
            "orthogonal vectors should have distance 1.0, got {dist}"
        );
    }

    #[test]
    fn test_hnsw_index_rebuild_and_search() {
        let mut index = HnswIndex::new();
        assert!(index.is_stale(1));

        // Create some test entries with embeddings
        let entries = vec![
            ("key1".to_string(), vec![1.0, 0.0, 0.0], b"val1".to_vec()),
            ("key2".to_string(), vec![0.9, 0.1, 0.0], b"val2".to_vec()),
            ("key3".to_string(), vec![0.0, 1.0, 0.0], b"val3".to_vec()),
        ];

        index.rebuild(&entries, 1);
        assert!(!index.is_stale(1));
        assert!(index.is_stale(2));

        // Search for something close to key1 (values are cached in the index now)
        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert!(!results.is_empty(), "should find results");
        assert_eq!(results[0].key, "key1", "closest result should be key1");
    }

    #[test]
    fn test_hnsw_index_empty() {
        let mut index = HnswIndex::new();
        index.rebuild(&[], 1);

        let results = index.search(&[1.0, 0.0], 10);
        assert!(results.is_empty(), "empty index should return no results");
    }
}
