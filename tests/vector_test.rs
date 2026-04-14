//! Integration tests for vector search functionality.
//!
//! Tests the HTTP search endpoint, embedding-aware store, and the engine-level
//! search_embedding API.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::http::ready::ReadyState;
use basalt::http::server::app;
use basalt::store::{ConsolidationManager, KvEngine};
use basalt::store::memory_type::MemoryType;
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

    // Give the server a moment to start accepting connections.
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
// Engine-level vector search tests
// ---------------------------------------------------------------------------

#[test]
fn test_set_with_embedding_and_search() {
    let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));

    // Store entries with embeddings in the "vectors" namespace
    engine
        .set_with_embedding(
            "vectors:a",
            b"value_a".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![1.0, 0.0, 0.0]),
        )
        .unwrap();
    engine
        .set_with_embedding(
            "vectors:b",
            b"value_b".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![0.9, 0.1, 0.0]),
        )
        .unwrap();
    engine
        .set_with_embedding(
            "vectors:c",
            b"value_c".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![0.0, 1.0, 0.0]),
        )
        .unwrap();

    // Search for vectors close to [1, 0, 0]
    let results = engine.search_embedding("vectors", &[1.0, 0.0, 0.0], 3);

    // Should get results
    assert!(!results.is_empty(), "should find results");

    // The closest result should be key "a" (identical vector)
    assert_eq!(results[0].key, "a", "closest result should be 'a'");
    assert!(
        results[0].distance < 0.01,
        "distance to identical vector should be near 0, got {}",
        results[0].distance
    );

    // Results should be sorted by distance (ascending)
    for i in 1..results.len() {
        assert!(
            results[i].distance >= results[i - 1].distance,
            "results should be sorted by distance: {:?}",
            results
                .iter()
                .map(|r| (r.key.clone(), r.distance))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn test_search_empty_namespace() {
    let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));

    // Search in a namespace with no entries
    let results = engine.search_embedding("empty", &[1.0, 0.0], 10);
    assert!(
        results.is_empty(),
        "empty namespace should return no results"
    );
}

#[test]
fn test_search_no_embeddings_in_namespace() {
    let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));

    // Store entries WITHOUT embeddings
    engine
        .set("noemb:x", b"val".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("noemb:y", b"val2".to_vec(), None, MemoryType::Semantic)
        .unwrap();

    let results = engine.search_embedding("noemb", &[1.0, 0.0], 10);
    assert!(
        results.is_empty(),
        "namespace with no embeddings should return no results"
    );
}

#[test]
fn test_search_respects_top_k() {
    let engine = KvEngine::new(4, Arc::new(ConsolidationManager::disabled()));

    // Store 5 entries with embeddings
    for i in 0..5u32 {
        let key = format!("topk:{}", i);
        let val = format!("val_{}", i);
        engine
            .set_with_embedding(
                &key,
                val.into_bytes(),
                None,
                MemoryType::Semantic,
                Some(vec![1.0, i as f32 * 0.1, 0.0]),
            )
            .unwrap();
    }

    // Request only top 2
    let results = engine.search_embedding("topk", &[1.0, 0.0, 0.0], 2);
    assert_eq!(results.len(), 2, "should return exactly top_k=2 results");
}

// ---------------------------------------------------------------------------
// HTTP search endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_search_endpoint() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store entries with embeddings via HTTP
    c.post(format!("{base}/store/vns"))
        .json(&serde_json::json!({
            "key": "doc1",
            "value": "document about cats",
            "embedding": [1.0, 0.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/vns"))
        .json(&serde_json::json!({
            "key": "doc2",
            "value": "document about dogs",
            "embedding": [0.0, 1.0, 0.0]
        }))
        .send()
        .await
        .unwrap();

    c.post(format!("{base}/store/vns"))
        .json(&serde_json::json!({
            "key": "doc3",
            "value": "document about both",
            "embedding": [0.9, 0.1, 0.0]
        }))
        .send()
        .await
        .unwrap();

    // Give a tiny delay for index consistency
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Search for vectors close to [1, 0, 0]
    let resp = c
        .post(format!("{base}/store/vns/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0, 0.0],
            "top_k": 3
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert!(!results.is_empty(), "should find results");

    // Closest should be doc1
    assert_eq!(results[0]["key"].as_str().unwrap(), "doc1");

    // Verify distances are sorted ascending
    let distances: Vec<f64> = results
        .iter()
        .map(|r| r["distance"].as_f64().unwrap())
        .collect();
    for i in 1..distances.len() {
        assert!(
            distances[i] >= distances[i - 1],
            "results should be sorted by distance: {:?}",
            distances
        );
    }
}

#[tokio::test]
async fn test_http_search_empty_namespace() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    let resp = c
        .post(format!("{base}/store/nonexistent/search"))
        .json(&serde_json::json!({
            "embedding": [1.0, 0.0],
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
        "empty namespace should return empty results"
    );
}

#[tokio::test]
async fn test_http_store_with_embedding() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    // Store with embedding
    let resp = c
        .post(format!("{base}/store/emb-ns"))
        .json(&serde_json::json!({
            "key": "emb-key",
            "value": "embedded value",
            "embedding": [0.5, 0.5, 0.5]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify we can still retrieve it normally
    let resp = c
        .get(format!("{base}/store/emb-ns/emb-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["value"].as_str().unwrap(), "embedded value");
}

#[tokio::test]
async fn test_http_batch_store_with_embeddings() {
    let (base, _handle, _engine) = start_default_server().await;
    let c = client();

    let batch = serde_json::json!({
        "memories": [
            {"key": "b1", "value": "batch1", "embedding": [1.0, 0.0, 0.0]},
            {"key": "b2", "value": "batch2", "embedding": [0.0, 1.0, 0.0]},
            {"key": "b3", "value": "batch3"},
        ]
    });

    let resp = c
        .post(format!("{base}/store/batch-emb-ns/batch"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["stored"].as_u64().unwrap(), 3);

    // Search should find the embedded entries
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let resp = c
        .post(format!("{base}/store/batch-emb-ns/search"))
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
    // b3 has no embedding, so only b1 and b2 should appear
    assert_eq!(
        results.len(),
        2,
        "only entries with embeddings should appear in search"
    );
}

// ---------------------------------------------------------------------------
// RESP VSEARCH command tests
// ---------------------------------------------------------------------------

#[test]
fn test_vsearch_command() {
    use basalt::resp::commands::CommandHandler;
    use basalt::resp::parser::Command;

    let handler = CommandHandler::new(Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled()))), None);

    // Store entries with embeddings using set_with_embedding through the engine
    let _engine = &handler;

    // First, test the error cases
    let cmd = Command {
        name: "VSEARCH".to_string(),
        args: vec![],
    };
    let resp = handler.handle(&cmd);
    match resp {
        basalt::resp::parser::RespValue::Error(s) => {
            assert!(s.contains("wrong number of arguments"), "got: {s}");
        }
        _ => panic!("expected error response"),
    }

    // Test with invalid embedding JSON
    let cmd = Command {
        name: "VSEARCH".to_string(),
        args: vec![b"myns".to_vec(), b"not-json".to_vec()],
    };
    let resp = handler.handle(&cmd);
    match resp {
        basalt::resp::parser::RespValue::Error(s) => {
            assert!(s.contains("invalid embedding JSON"), "got: {s}");
        }
        _ => panic!("expected error response for invalid JSON"),
    }
}

#[test]
fn test_vsearch_with_results() {
    use basalt::resp::commands::CommandHandler;
    use basalt::resp::parser::{Command, RespValue};

    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));

    // Store entries with embeddings
    engine
        .set_with_embedding(
            "vtest:a",
            b"value_a".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![1.0, 0.0, 0.0]),
        )
        .unwrap();
    engine
        .set_with_embedding(
            "vtest:b",
            b"value_b".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![0.0, 1.0, 0.0]),
        )
        .unwrap();

    let handler = CommandHandler::new(engine, None);

    // VSEARCH vtest [1.0, 0.0, 0.0] COUNT 2
    let cmd = Command {
        name: "VSEARCH".to_string(),
        args: vec![
            b"vtest".to_vec(),
            b"[1.0, 0.0, 0.0]".to_vec(),
            b"COUNT".to_vec(),
            b"2".to_vec(),
        ],
    };
    let resp = handler.handle(&cmd);

    match resp {
        RespValue::Array(Some(items)) => {
            // Should be 2 results * 3 fields each = 6 items
            assert_eq!(items.len(), 6, "should have 6 items (2 results x 3 fields)");
            // First result: key="a", distance near 0, value="value_a"
            match &items[0] {
                RespValue::BulkString(Some(key)) => {
                    assert_eq!(key, b"a");
                }
                _ => panic!("expected bulk string for key"),
            }
            match &items[2] {
                RespValue::BulkString(Some(val)) => {
                    assert_eq!(val, b"value_a");
                }
                _ => panic!("expected bulk string for value"),
            }
        }
        _ => panic!("expected array response, got: {:?}", resp),
    }
}
