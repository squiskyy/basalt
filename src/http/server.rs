use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;
use crate::store::shard::ShardFullError;

use super::auth::{auth_middleware, AuthStore};
use super::models::{
    BatchGetRequest, BatchGetResponse, BatchStoreRequest, BatchStoreResponse, HealthResponse,
    InfoResponse, ListQuery, ListResponse, SearchRequest, SearchResponse, SearchResult,
    SimpleResponse, StoreRequest, StoreResponse,
};

/// Shared application state wrapping the KV engine, auth store, and optional db_path.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<KvEngine>,
    pub auth: Arc<AuthStore>,
    pub db_path: Option<String>,
    pub compression_threshold: usize,
}

/// Build the axum Router with all routes, auth middleware, and shared state.
pub fn app(engine: Arc<KvEngine>, auth: Arc<AuthStore>, db_path: Option<String>, compression_threshold: usize) -> Router {
    let state = AppState {
        engine,
        auth,
        db_path,
        compression_threshold,
    };

    // Public routes (no auth)
    let public = Router::new()
        .route("/health", get(health))
        .route("/info", get(info));

    // Protected routes (auth middleware)
    let protected = Router::new()
        .route("/store/{namespace}", post(store_memory))
        .route("/store/{namespace}", get(list_memories))
        .route("/store/{namespace}", delete(delete_namespace))
        .route("/store/{namespace}/search", post(search_memories))
        .route("/store/{namespace}/batch", post(batch_store))
        .route("/store/{namespace}/batch/get", post(batch_get))
        .route("/store/{namespace}/{key}", get(get_memory))
        .route("/store/{namespace}/{key}", delete(delete_memory))
        .route("/snapshot", post(trigger_snapshot))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .merge(public)
        .merge(protected)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state)
}

// --- Handlers ---

async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    let resp = HealthResponse {
        status: "ok".to_string(),
    };
    (StatusCode::OK, axum::Json(resp))
}

async fn info(State(state): State<AppState>) -> impl IntoResponse {
    let resp = InfoResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        shards: state.engine.shard_count(),
        compression_threshold: state.engine.compression_threshold(),
    };
    (StatusCode::OK, axum::Json(resp))
}

async fn store_memory(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(body): axum::Json<StoreRequest>,
) -> impl IntoResponse {
    let internal_key = format!("{}:{}", namespace, body.key);
    let memory_type = body.r#type.unwrap_or(MemoryType::Semantic);
    let value_bytes = body.value.into_bytes();

    let result = if body.embedding.is_some() {
        state
            .engine
            .set_with_embedding(&internal_key, value_bytes, body.ttl_ms, memory_type, body.embedding)
    } else {
        state
            .engine
            .set(&internal_key, value_bytes, body.ttl_ms, memory_type)
    };

    match result {
        Ok(()) => {
            let resp = SimpleResponse {
                ok: true,
                deleted: None,
            };
            (StatusCode::CREATED, axum::Json(resp)).into_response()
        }
        Err(ShardFullError { max_entries, current }) => {
            let body = serde_json::json!({
                "error": "max entries exceeded",
                "max_entries": max_entries,
                "current": current,
            });
            (StatusCode::INSUFFICIENT_STORAGE, axum::Json(body)).into_response()
        }
    }
}

async fn list_memories(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let prefix = format!("{}:", namespace);
    let entries = state.engine.scan_prefix(&prefix);

    let prefix_stripped = &prefix;
    let memories: Vec<StoreResponse> = entries
        .into_iter()
        .filter(|(_, _, meta)| {
            if let Some(ref want_type) = query.r#type {
                meta.memory_type == *want_type
            } else {
                true
            }
        })
        .map(|(key, value, meta)| {
            let display_key = key
                .strip_prefix(prefix_stripped)
                .unwrap_or(&key)
                .to_string();
            (display_key, value, meta)
        })
        .filter(|(display_key, _, _)| {
            if let Some(ref qprefix) = query.prefix {
                display_key.starts_with(qprefix.as_str())
            } else {
                true
            }
        })
        .map(|(display_key, value, meta)| StoreResponse {
            key: display_key,
            value: String::from_utf8_lossy(&value).to_string(),
            r#type: Some(meta.memory_type.to_string()),
            ttl_ms: meta.ttl_remaining_ms,
        })
        .collect();

    let resp = ListResponse { memories };
    (StatusCode::OK, axum::Json(resp))
}

async fn get_memory(
    State(state): State<AppState>,
    Path((namespace, key)): Path<(String, String)>,
) -> impl IntoResponse {
    let internal_key = format!("{}:{}", namespace, key);

    match state.engine.get_with_meta(&internal_key) {
        Some((value, meta)) => {
            let resp = StoreResponse {
                key,
                value: String::from_utf8_lossy(&value).to_string(),
                r#type: Some(meta.memory_type.to_string()),
                ttl_ms: meta.ttl_remaining_ms,
            };
            (StatusCode::OK, axum::Json(resp)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(SimpleResponse {
                ok: false,
                deleted: None,
            }),
        )
            .into_response(),
    }
}

async fn delete_memory(
    State(state): State<AppState>,
    Path((namespace, key)): Path<(String, String)>,
) -> impl IntoResponse {
    let internal_key = format!("{}:{}", namespace, key);
    let deleted = state.engine.delete(&internal_key);

    if deleted {
        (
            StatusCode::OK,
            axum::Json(SimpleResponse {
                ok: true,
                deleted: None,
            }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(SimpleResponse {
                ok: false,
                deleted: None,
            }),
        )
    }
}

async fn delete_namespace(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> impl IntoResponse {
    let prefix = format!("{}:", namespace);
    let count = state.engine.delete_prefix(&prefix);

    (
        StatusCode::OK,
        axum::Json(SimpleResponse {
            ok: true,
            deleted: Some(count),
        }),
    )
}

// --- Batch handlers ---

async fn batch_store(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(body): axum::Json<BatchStoreRequest>,
) -> impl IntoResponse {
    let mut stored = 0usize;
    for item in &body.memories {
        let internal_key = format!("{}:{}", namespace, item.key);
        let memory_type = item.r#type.unwrap_or(MemoryType::Semantic);
        let value_bytes = item.value.as_bytes();
        let result = if item.embedding.is_some() {
            state
                .engine
                .set_with_embedding(&internal_key, value_bytes.to_vec(), item.ttl_ms, memory_type, item.embedding.clone())
        } else {
            state
                .engine
                .set(&internal_key, value_bytes.to_vec(), item.ttl_ms, memory_type)
        };
        match result {
            Ok(()) => stored += 1,
            Err(ShardFullError { max_entries, current }) => {
                let body = serde_json::json!({
                    "error": "max entries exceeded",
                    "max_entries": max_entries,
                    "current": current,
                    "stored": stored,
                });
                return (StatusCode::INSUFFICIENT_STORAGE, axum::Json(body)).into_response();
            }
        }
    }

    let resp = BatchStoreResponse { ok: true, stored };
    (StatusCode::CREATED, axum::Json(resp)).into_response()
}

async fn batch_get(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(body): axum::Json<BatchGetRequest>,
) -> impl IntoResponse {
    let mut memories = Vec::with_capacity(body.keys.len());
    let mut missing = Vec::new();

    for key in &body.keys {
        let internal_key = format!("{}:{}", namespace, key);
        match state.engine.get_with_meta(&internal_key) {
            Some((value, meta)) => {
                memories.push(StoreResponse {
                    key: key.clone(),
                    value: String::from_utf8_lossy(&value).to_string(),
                    r#type: Some(meta.memory_type.to_string()),
                    ttl_ms: meta.ttl_remaining_ms,
                });
            }
            None => {
                missing.push(key.clone());
            }
        }
    }

    let resp = BatchGetResponse {
        memories,
        missing: if missing.is_empty() {
            None
        } else {
            Some(missing)
        },
    };
    (StatusCode::OK, axum::Json(resp))
}

// --- Search handler ---

async fn search_memories(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(req): axum::Json<SearchRequest>,
) -> impl IntoResponse {
    let results = state.engine.search_embedding(&namespace, &req.embedding, req.top_k);
    let search_results: Vec<SearchResult> = results
        .into_iter()
        .map(|r| SearchResult {
            key: r.key,
            distance: r.distance,
            value: String::from_utf8_lossy(&r.value).to_string(),
        })
        .collect();
    let resp = SearchResponse {
        results: search_results,
    };
    (StatusCode::OK, axum::Json(resp))
}

// --- Snapshot handler ---

async fn trigger_snapshot(State(state): State<AppState>) -> impl IntoResponse {
    match &state.db_path {
        Some(db_path) => {
            let path = std::path::Path::new(db_path);
            match crate::store::persistence::snapshot_with_threshold(path, &state.engine, 3, state.compression_threshold) {
                Ok(snapshot_path) => {
                    let entries = crate::store::persistence::collect_entries(&state.engine).len();
                    let resp = SnapshotResponse {
                        ok: true,
                        path: snapshot_path.to_string_lossy().to_string(),
                        entries,
                    };
                    (StatusCode::OK, axum::Json(resp)).into_response()
                }
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(SimpleResponse {
                        ok: false,
                        deleted: None,
                    }),
                )
                    .into_response(),
            }
        }
        None => (
            StatusCode::PRECONDITION_FAILED,
            axum::Json(SimpleResponse {
                ok: false,
                deleted: None,
            }),
        )
            .into_response(),
    }
}

#[derive(serde::Serialize)]
struct SnapshotResponse {
    ok: bool,
    path: String,
    entries: usize,
}
