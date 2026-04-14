//! Vector search integration tests (Issue #30).
//!
//! These tests exercise the full search endpoint lifecycle: store entries with
//! embeddings via HTTP, search via POST /store/{ns}/search, and verify results
//! are sorted by similarity. They complement the existing unit-level and
//! engine-level vector tests in vector_test.rs.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::http::ready::ReadyState;
use basalt::http::server::app;
use basalt::store::{ConsolidationManager, KvEngine};
use basalt::store::share::ShareStore;

use axum::serve;
use reqwest::{Client, StatusCode};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spin up the server on a random port.
async fn start_server(
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let router = app(
        engine,
        auth,
        Arc::new(ShareStore::new()),
        db_path,
        1024,
        None,
        Arc::new(ReadyState::new_ready()),
        basalt::metrics::create_metrics(),
    );

    let handle = tokio::spawn(async move {
        serve(listener, router).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (base, handle)
}

/// Convenience: start a server with no auth and no db_path.
async fn start_default_server() -> (String, tokio::task::JoinHandle<()>, Arc<KvEngine>) {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let auth = Arc::new(AuthStore::new());
    let (base, handle) = start_server(engine.clone(), auth, None).await;
    (base, handle, engine)
}

fn client() -> Client {
    Client::new()
}

// ---------------------------------------------------------------------------
// Issue #30: Full vector search integration tests
// ---------------------------------------------------------------------------

/// Test the full lifecycle: store entries with embeddings, search, verify
/// results are sorted by cosine distance and the closest match is correct.
#[tokio::test]
async fn test_search_lifecycle_store_search_verify_sort() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store three documents with distinct embeddings
    c.post(format!("{base}/store/lifecycle"))
        .json(&serde_json::json!({
            "key": "cat",
            "value": "feline pet",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/lifecycle"))
        .json(&serde_json::json!({
            "key": "dog",
            "value": "canine pet",
            "embedding": [0.0, 1.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/lifecycle"))
        .json(&serde_json::json!({
            "key": "car",
            "value": "automobile vehicle",
            "embedding": [0.0, 0.0, 1.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search near the "cat" embedding
    let resp = c
        .post(format!("{base}/store/lifecycle/search"))
        .json(&serde_json::json!({
            "embedding": [0.9, 0.1, 0.0],
            "top_k": 3
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "should return all 3 results");

    // Closest result should be "cat"
    assert_eq!(
        results[0]["key"].as_str().unwrap(),
        "cat",
        "closest result should be 'cat'"
    );

    // Results must be sorted by distance (ascending)
    let distances: Vec<f64> = results
        .iter()
        .map(|r| r["distance"].as_f64().unwrap())
        .collect();
    for i in 1..distances.len() {
        assert!(
            distances[i] >= distances[i - 1],
            "results should be sorted by distance ascending: {:?}",
            distances
        );
    }

    // "car" should be the farthest from [0.9, 0.1, 0.0]
    assert_eq!(
        results[2]["key"].as_str().unwrap(),
        "car",
        "farthest result should be 'car'"
    );
}

/// Test that search returns the correct number of results when top_k is
/// smaller than the total number of embedded entries.
#[tokio::test]
async fn test_search_top_k_limits_results() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store 5 entries with embeddings
    for i in 0..5u32 {
        let key = format!("doc{i}");
        let value = format!("document {i}");
        c.post(format!("{base}/store/topk"))
            .json(&serde_json::json!({
                "key": key,
                "value": value,
                "embedding": [1.0, i as f32 * 0.1, 0.0]
            }))
            .send()
            .await
            .unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Request only top 2
    let resp = c
        .post(format!("{base}/store/topk/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 2
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "top_k=2 should return exactly 2 results");
}

/// Test that entries without embeddings are excluded from search results.
#[tokio::test]
async fn test_search_excludes_entries_without_embeddings() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store one with embedding, one without
    c.post(format!("{base}/store/mixed"))
        .json(&serde_json::json!({
            "key": "with_emb",
            "value": "has embedding",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/mixed"))
        .json(&serde_json::json!({
            "key": "no_emb",
            "value": "no embedding here"
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let resp = c
        .post(format!("{base}/store/mixed/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "only embedded entries should appear in search"
    );
    assert_eq!(results[0]["key"].as_str().unwrap(), "with_emb");
}

/// Test that searching an empty namespace returns an empty results array.
#[tokio::test]
async fn test_search_empty_namespace_returns_empty() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    let resp = c
        .post(format!("{base}/store/empty-ns/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 5
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert!(
        results.is_empty(),
        "empty namespace should return no results"
    );
}

/// Test that updating an entry with a new embedding changes search results.
#[tokio::test]
async fn test_search_after_embedding_update() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store entry with embedding near [1,0,0]
    c.post(format!("{base}/store/update"))
        .json(&serde_json::json!({
            "key": "mutable",
            "value": "will be updated",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search should find it near [1,0,0]
    let resp = c
        .post(format!("{base}/store/update/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let first_dist = results[0]["distance"].as_f64().unwrap();
    assert!(
        first_dist < 0.01,
        "distance to identical vector should be near 0, got {first_dist}"
    );

    // Now update the same key with a different embedding near [0,0,1]
    c.post(format!("{base}/store/update"))
        .json(&serde_json::json!({
            "key": "mutable",
            "value": "updated value",
            "embedding": [0.0, 0.0, 1.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search near [1,0,0] - the updated entry should now be far
    let resp = c
        .post(format!("{base}/store/update/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let updated_dist = results[0]["distance"].as_f64().unwrap();
    assert!(
        updated_dist > first_dist,
        "updated distance should be larger after changing embedding to orthogonal vector: before={}, after={}",
        first_dist,
        updated_dist
    );
}

/// Test that deleting an entry removes it from search results.
#[tokio::test]
async fn test_search_after_delete() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store two entries
    c.post(format!("{base}/store/del-search"))
        .json(&serde_json::json!({
            "key": "keep",
            "value": "keep this",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/del-search"))
        .json(&serde_json::json!({
            "key": "remove",
            "value": "delete this",
            "embedding": [0.9, 0.1, 0.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Verify both appear in search
    let resp = c
        .post(format!("{base}/store/del-search/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 2);

    // Delete "remove"
    let resp = c
        .delete(format!("{base}/store/del-search/remove"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Search again - only "keep" should remain
    let resp = c
        .post(format!("{base}/store/del-search/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "only one result should remain after delete"
    );
    assert_eq!(results[0]["key"].as_str().unwrap(), "keep");
}

/// Test batch store with embeddings followed by search.
#[tokio::test]
async fn test_search_after_batch_store_with_embeddings() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    let batch = serde_json::json!({
        "memories": [
            {"key": "alpha", "value": "first doc", "embedding": [1.0, 0.0, 0.0]},
            {"key": "beta", "value": "second doc", "embedding": [0.0, 1.0, 0.0]},
            {"key": "gamma", "value": "third doc", "embedding": [0.0, 0.0, 1.0]},
            {"key": "delta", "value": "no embedding doc"},
        ]
    });

    let resp = c
        .post(format!("{base}/store/batch-search/batch"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search should only return the 3 embedded entries
    let resp = c
        .post(format!("{base}/store/batch-search/search"))
        .json(&serde_json::json!({
            "embedding": [0.0, 1.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        3,
        "only embedded entries should appear in search"
    );

    // "beta" should be the closest match
    assert_eq!(results[0]["key"].as_str().unwrap(), "beta");
}

/// Test search across multiple different namespaces - entries in one namespace
/// should not leak into another namespace's search results.
#[tokio::test]
async fn test_search_namespace_isolation() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store in namespace "animals"
    c.post(format!("{base}/store/animals"))
        .json(&serde_json::json!({
            "key": "cat",
            "value": "feline",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    // Store in namespace "vehicles"
    c.post(format!("{base}/store/vehicles"))
        .json(&serde_json::json!({
            "key": "car",
            "value": "automobile",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search "animals" namespace
    let resp = c
        .post(format!("{base}/store/animals/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "animals namespace should have 1 result");
    assert_eq!(results[0]["key"].as_str().unwrap(), "cat");

    // Search "vehicles" namespace
    let resp = c
        .post(format!("{base}/store/vehicles/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 10
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "vehicles namespace should have 1 result");
    assert_eq!(results[0]["key"].as_str().unwrap(), "car");
}

/// Test that search result values are correctly returned as the stored string.
#[tokio::test]
async fn test_search_returns_correct_values() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    c.post(format!("{base}/store/val-ns"))
        .json(&serde_json::json!({
            "key": "doc1",
            "value": "the quick brown fox",
            "embedding": [0.5, 0.5, 0.0]
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let resp = c
        .post(format!("{base}/store/val-ns/search"))
        .json(&serde_json::json!({
            "embedding": [0.5, 0.5, 0.0],
            "top_k": 5
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["value"].as_str().unwrap(), "the quick brown fox");
    assert_eq!(results[0]["key"].as_str().unwrap(), "doc1");
    // Distance to identical vector should be 0
    let dist = results[0]["distance"].as_f64().unwrap();
    assert!(
        dist < 0.001,
        "distance to identical vector should be ~0, got {dist}"
    );
}
