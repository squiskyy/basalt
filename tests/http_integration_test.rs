//! HTTP integration tests for Basalt.
//!
//! These tests spin up the actual axum server on a random port and exercise
//! all HTTP endpoints via reqwest. They cover health, info, store/get/delete,
//! memory types & TTL, batch operations, auth, snapshot, and capacity limits.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::http::server::app;
use basalt::store::engine::KvEngine;

use axum::serve;
use reqwest::{Client, StatusCode};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spin up the server on a random port, returning the base URL and a
/// JoinHandle for the server task. The server shuts down when the handle
/// is dropped (or the test process exits).
async fn start_server(
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let router = app(engine, auth, db_path, 1024, None);
    let handle = tokio::spawn(async move {
        serve(listener, router).await.unwrap();
    });

    // Give the server a moment to start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (base, handle)
}

/// Convenience: start a server with no auth and no db_path.
async fn start_default_server() -> (String, tokio::task::JoinHandle<()>, Arc<KvEngine>) {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::new());
    let (base, handle) = start_server(engine.clone(), auth, None).await;
    (base, handle, engine)
}

/// Build a reqwest client.
fn client() -> Client {
    Client::new()
}

// ---------------------------------------------------------------------------
// a. Health endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_returns_200() {
    let (base, _handle, _engine) = start_default_server().await;
    let resp = client().get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

// ---------------------------------------------------------------------------
// b. Info endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_info_returns_200_with_json() {
    let (base, _handle, _engine) = start_default_server().await;
    let resp = client().get(format!("{base}/info")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    // Version should be a non-empty string
    assert!(body["version"].is_string());
    assert!(!body["version"].as_str().unwrap().is_empty());
    // Shards should be a number (we created 4, rounded to power-of-2 = 4)
    assert!(body["shards"].is_number());
}

// ---------------------------------------------------------------------------
// c. Store and retrieve a memory (POST then GET)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_and_retrieve() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store
    let store_body = serde_json::json!({
        "key": "my-key",
        "value": "my-value",
        "type": "semantic",
    });
    let resp = c
        .post(format!("{base}/store/test-ns"))
        .json(&store_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());

    // Retrieve
    let resp = c
        .get(format!("{base}/store/test-ns/my-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["key"], "my-key");
    assert_eq!(body["value"], "my-value");
    assert_eq!(body["type"], "semantic");
}

// ---------------------------------------------------------------------------
// d. Store with memory types and verify TTL behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_memory_types_and_ttl() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Episodic with short TTL
    let resp = c
        .post(format!("{base}/store/mem-ns"))
        .json(&serde_json::json!({
            "key": "episodic-short",
            "value": "ephemeral data",
            "type": "episodic",
            "ttl_ms": 50,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Semantic with no TTL
    let resp = c
        .post(format!("{base}/store/mem-ns"))
        .json(&serde_json::json!({
            "key": "semantic-perm",
            "value": "permanent fact",
            "type": "semantic",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Procedural with no TTL
    let resp = c
        .post(format!("{base}/store/mem-ns"))
        .json(&serde_json::json!({
            "key": "proc-perm",
            "value": "skill data",
            "type": "procedural",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify all three are retrievable immediately
    let resp = c
        .get(format!("{base}/store/mem-ns/episodic-short"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "episodic");
    // TTL should be present and near 50ms
    assert!(body["ttl_ms"].is_number());

    let resp = c
        .get(format!("{base}/store/mem-ns/semantic-perm"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "semantic");
    // No TTL for semantic
    assert!(body["ttl_ms"].is_null());

    let resp = c
        .get(format!("{base}/store/mem-ns/proc-perm"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "procedural");
    assert!(body["ttl_ms"].is_null());

    // Wait for the episodic entry to expire
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = c
        .get(format!("{base}/store/mem-ns/episodic-short"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Semantic and procedural should still exist
    let resp = c
        .get(format!("{base}/store/mem-ns/semantic-perm"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = c
        .get(format!("{base}/store/mem-ns/proc-perm"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// e. Delete a key (DELETE /store/{namespace}/{key})
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_key() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store a key
    c.post(format!("{base}/store/del-ns"))
        .json(&serde_json::json!({"key": "to-delete", "value": "v"}))
        .send()
        .await
        .unwrap();

    // Delete it
    let resp = c
        .delete(format!("{base}/store/del-ns/to-delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());

    // Verify it's gone
    let resp = c
        .get(format!("{base}/store/del-ns/to-delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Delete non-existent key returns 404
    let resp = c
        .delete(format!("{base}/store/del-ns/no-such-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// f. Delete prefix (DELETE /store/{namespace} — deletes all keys in namespace)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_prefix() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store several keys in a namespace
    for i in 0..5u8 {
        let key = format!("key{i}");
        let value = format!("val{i}");
        c.post(format!("{base}/store/prefix-ns"))
            .json(&serde_json::json!({"key": key, "value": value}))
            .send()
            .await
            .unwrap();
    }

    // Also store a key in a different namespace to verify it's NOT deleted
    c.post(format!("{base}/store/other-ns"))
        .json(&serde_json::json!({"key": "safe-key", "value": "safe"}))
        .send()
        .await
        .unwrap();

    // Delete the entire prefix-ns namespace
    let resp = c
        .delete(format!("{base}/store/prefix-ns"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["deleted"].as_u64().unwrap(), 5);

    // Keys in prefix-ns should be gone
    let resp = c
        .get(format!("{base}/store/prefix-ns/key0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Key in other-ns should still exist
    let resp = c
        .get(format!("{base}/store/other-ns/safe-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// g. Batch store (POST /store/{namespace}/batch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_store() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    let batch = serde_json::json!({
        "memories": [
            {"key": "a", "value": "alpha", "type": "semantic"},
            {"key": "b", "value": "beta", "type": "episodic", "ttl_ms": 60000},
            {"key": "c", "value": "gamma", "type": "procedural"},
        ]
    });

    let resp = c
        .post(format!("{base}/store/batch-ns/batch"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["stored"].as_u64().unwrap(), 3);

    // Verify we can retrieve each one
    for key in &["a", "b", "c"] {
        let resp = c
            .get(format!("{base}/store/batch-ns/{key}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

// ---------------------------------------------------------------------------
// h. Batch get (POST /store/{namespace}/batch/get)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_get() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Pre-store some entries
    c.post(format!("{base}/store/bget-ns"))
        .json(&serde_json::json!({"key": "x", "value": "ex", "type": "semantic"}))
        .send()
        .await
        .unwrap();
    c.post(format!("{base}/store/bget-ns"))
        .json(&serde_json::json!({"key": "y", "value": "why", "type": "procedural"}))
        .send()
        .await
        .unwrap();

    // Batch get: two found, one missing
    let req = serde_json::json!({
        "keys": ["x", "y", "z"]
    });
    let resp = c
        .post(format!("{base}/store/bget-ns/batch/get"))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 2);

    let missing = body["missing"].as_array().unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].as_str().unwrap(), "z");

    // Verify found entries have correct keys
    let found_keys: Vec<&str> = memories
        .iter()
        .map(|m| m["key"].as_str().unwrap())
        .collect();
    assert!(found_keys.contains(&"x"));
    assert!(found_keys.contains(&"y"));
}

// ---------------------------------------------------------------------------
// i. Auth: unauthenticated request returns 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_unauthenticated_returns_401() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::from_list(vec![(
        "bsk-secret".to_string(),
        vec!["*".to_string()],
    )]));
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // No Authorization header → 401
    let resp = c
        .post(format!("{base}/store/secure-ns"))
        .json(&serde_json::json!({"key": "k", "value": "v"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// j. Auth: wrong namespace returns 403
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_wrong_namespace_returns_403() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::from_list(vec![(
        "bsk-ns-a".to_string(),
        vec!["ns-a".to_string()],
    )]));
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // Token bsk-ns-a is only authorized for ns-a, not ns-b
    let resp = c
        .post(format!("{base}/store/ns-b"))
        .header("Authorization", "Bearer bsk-ns-a")
        .json(&serde_json::json!({"key": "k", "value": "v"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// k. Auth: correct token+namespace returns 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_correct_token_namespace_returns_200() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::from_list(vec![(
        "bsk-ns-a".to_string(),
        vec!["ns-a".to_string()],
    )]));
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // Correct token for ns-a → store succeeds (201)
    let resp = c
        .post(format!("{base}/store/ns-a"))
        .header("Authorization", "Bearer bsk-ns-a")
        .json(&serde_json::json!({"key": "k", "value": "v"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET also requires auth
    let resp = c
        .get(format!("{base}/store/ns-a/k"))
        .header("Authorization", "Bearer bsk-ns-a")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["value"], "v");
}

// ---------------------------------------------------------------------------
// l. Snapshot endpoint (POST /snapshot)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_snapshot_without_db_path_returns_412() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // No db_path configured → PRECONDITION_FAILED (412)
    let resp = c.post(format!("{base}/snapshot")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(!body["ok"].as_bool().unwrap());
}

#[tokio::test]
async fn test_snapshot_with_db_path_returns_200() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::new());
    let db_dir = std::env::temp_dir().join(format!("basalt_test_snapshot_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir).unwrap();

    let (base, _handle) = start_server(engine, auth, Some(db_dir.to_string_lossy().to_string())).await;
    let c = client();

    // Store something so the snapshot has data
    c.post(format!("{base}/store/snap-ns"))
        .json(&serde_json::json!({"key": "s1", "value": "snapshot-me"}))
        .send()
        .await
        .unwrap();

    let resp = c.post(format!("{base}/snapshot")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert!(body["path"].is_string());
    assert!(body["entries"].as_u64().unwrap() >= 1);

    // Clean up
    let _ = std::fs::remove_dir_all(&db_dir);
}

// ---------------------------------------------------------------------------
// m. 507 Insufficient Storage when max entries exceeded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_507_insufficient_storage() {
    // Create a tiny engine: 1 shard, max 2 entries
    let engine = Arc::new(KvEngine::with_max_entries(1, 2));
    let auth = Arc::new(AuthStore::new());
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // Fill the shard (2 entries)
    for i in 0..2u8 {
        let key = format!("k{i}");
        let value = format!("v{i}");
        let resp = c
            .post(format!("{base}/store/cap-ns"))
            .json(&serde_json::json!({"key": key, "value": value}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // 3rd entry should be rejected with 507
    let resp = c
        .post(format!("{base}/store/cap-ns"))
        .json(&serde_json::json!({"key": "k2", "value": "overflow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "max entries exceeded");
    assert!(body["max_entries"].is_number());
    assert!(body["current"].is_number());
}

// ---------------------------------------------------------------------------
// Bonus: Auth with wildcard token accesses all namespaces
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_wildcard_token() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::from_list(vec![(
        "bsk-admin".to_string(),
        vec!["*".to_string()],
    )]));
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // Wildcard token should access any namespace
    let resp = c
        .post(format!("{base}/store/any-namespace"))
        .header("Authorization", "Bearer bsk-admin")
        .json(&serde_json::json!({"key": "k", "value": "v"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ---------------------------------------------------------------------------
// Bonus: Health and Info don't require auth even when auth is enabled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_and_info_no_auth_required() {
    let engine = Arc::new(KvEngine::new(4));
    let auth = Arc::new(AuthStore::from_list(vec![(
        "bsk-secret".to_string(),
        vec!["*".to_string()],
    )]));
    let (base, _handle) = start_server(engine, auth, None).await;
    let c = client();

    // /health and /info are public routes
    let resp = c.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = c.get(format!("{base}/info")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Bonus: GET /store/{namespace} lists entries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_memories() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store a few entries
    c.post(format!("{base}/store/list-ns"))
        .json(&serde_json::json!({"key": "alpha", "value": "a", "type": "semantic"}))
        .send()
        .await
        .unwrap();
    c.post(format!("{base}/store/list-ns"))
        .json(&serde_json::json!({"key": "beta", "value": "b", "type": "episodic", "ttl_ms": 60000}))
        .send()
        .await
        .unwrap();

    let resp = c
        .get(format!("{base}/store/list-ns"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 2);

    // Filter by type
    let resp = c
        .get(format!("{base}/store/list-ns?type=episodic"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["type"], "episodic");
}
