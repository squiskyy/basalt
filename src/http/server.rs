use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};

use crate::metrics::Metrics;
use crate::store::NamespacedKey;
use crate::store::consolidation::ConsolidationRule;
use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;
use crate::store::shard::ShardFullError;
use crate::store::share::{SharePolicy, ShareStore};
use crate::time::now_ms;

use super::auth::{AuthStore, auth_middleware};
use super::models::{
    BatchGetRequest, BatchGetResponse, BatchStoreRequest, BatchStoreResponse, ConsolidateQuery,
    ConsolidateResponse, ConsolidationStatusResponse, GrantShareRequest, InfoResponse, ListQuery,
    ListResponse, RegisterTriggerRequest, RevokeShareRequest, SearchRequest, SearchResponse,
    SearchResult, ShareListResponse, SimpleResponse, StoreRequest, StoreResponse,
    TriggerFireResponse, TriggerInfoResponse, TriggerListResponse,
};
use super::ready::{ReadyResponse, ReadyState};

/// Shared application state wrapping the KV engine, auth store, share store, and optional db_path.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<KvEngine>,
    pub auth: Arc<AuthStore>,
    pub share: Arc<ShareStore>,
    pub db_path: Option<String>,
    pub compression_threshold: usize,
    pub repl_state: Option<Arc<crate::replication::ReplicationState>>,
    pub ready_state: Arc<ReadyState>,
    pub metrics: Arc<dyn Metrics>,
    pub consolidation_interval_ms: u64,
}

/// Build the axum Router with all routes, auth middleware, and shared state.
#[allow(clippy::too_many_arguments)]
pub fn app(
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
    compression_threshold: usize,
    repl_state: Option<Arc<crate::replication::ReplicationState>>,
    ready_state: Arc<ReadyState>,
    metrics: Arc<dyn Metrics>,
    consolidation_interval_ms: u64,
) -> Router {
    let state = AppState {
        engine,
        auth,
        share,
        db_path,
        compression_threshold,
        repl_state,
        ready_state,
        metrics,
        consolidation_interval_ms,
    };

    // Public routes (no auth)
    let public = Router::new()
        .route("/health", get(health))
        .route("/ready", get(readiness))
        .route("/metrics", get(metrics_handler))
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
        .route("/share", post(grant_share))
        .route("/share", delete(revoke_share))
        .route("/share", get(list_granted))
        .route("/shared-with-me", get(list_shared_with_me))
        .route("/trigger", post(register_trigger))
        .route("/trigger", get(list_triggers))
        .route("/trigger/{id}", delete(delete_trigger))
        .route("/trigger/{id}/fire", post(fire_trigger))
        .route("/trigger/{id}/enable", post(enable_trigger))
        .route("/trigger/{id}/disable", post(disable_trigger))
        .route("/consolidate/{namespace}", post(consolidate))
        .route("/consolidate/status", get(consolidation_status))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .merge(public)
        .merge(protected)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state)
}

// --- Handlers ---

async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    // Liveness: always returns OK when the process is running
    let resp = ReadyResponse {
        status: "ok".to_string(),
        reason: None,
    };
    (StatusCode::OK, axum::Json(resp))
}

async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready_state.is_ready() {
        let resp = ReadyResponse {
            status: "ok".to_string(),
            reason: None,
        };
        (StatusCode::OK, axum::Json(resp))
    } else {
        let reason = state.ready_state.reason();
        let resp = ReadyResponse {
            status: "not_ready".to_string(),
            reason: Some(reason),
        };
        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(resp))
    }
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Refresh shard entry counts before rendering metrics
    let shard_count = state.engine.shard_count();
    for i in 0..shard_count {
        let count = state.engine.shard_entry_count(i);
        state.metrics.set_shard_entries(i, count);
    }

    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

async fn info(State(state): State<AppState>) -> impl IntoResponse {
    let shard_count = state.engine.shard_count();
    let mut shard_entries = Vec::with_capacity(shard_count);
    for i in 0..shard_count {
        shard_entries.push(state.engine.shard_entry_count(i));
    }
    let resp = InfoResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        shards: shard_count,
        compression_threshold: state.engine.compression_threshold(),
        eviction_policy: state.engine.eviction_policy().to_string(),
        max_entries_per_shard: state.engine.max_entries(),
        shard_entries,
    };
    (StatusCode::OK, axum::Json(resp))
}

async fn store_memory(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(body): axum::Json<StoreRequest>,
) -> impl IntoResponse {
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "write", &namespace);
    let nk = NamespacedKey::new(&namespace, &body.key);
    let internal_key = nk.to_internal();
    let memory_type = body.r#type.unwrap_or(MemoryType::Semantic);
    let value_bytes = body.value.into_bytes();

    let result = if body.embedding.is_some() {
        state.engine.set_with_embedding(
            &internal_key,
            value_bytes,
            body.ttl_ms,
            memory_type,
            body.embedding,
        )
    } else {
        state
            .engine
            .set(&internal_key, value_bytes, body.ttl_ms, memory_type)
    };

    match result {
        Ok(()) => {
            state.metrics.record_write(&namespace);
            let resp = SimpleResponse {
                ok: true,
                deleted: None,
            };
            (StatusCode::CREATED, axum::Json(resp)).into_response()
        }
        Err(ShardFullError {
            max_entries,
            current,
            shard_index,
        }) => {
            let body = serde_json::json!({
                "error": "max entries exceeded",
                "max_entries": max_entries,
                "current": current,
                "shard": shard_index,
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
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "list", &namespace);
    state.metrics.record_read(&namespace);
    let prefix = NamespacedKey::new(&namespace, "").prefix();
    let entries = if query.sort_by.as_deref() == Some("relevance") {
        state.engine.scan_prefix_sorted(&prefix)
    } else {
        state.engine.scan_prefix(&prefix)
    };

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
            relevance: Some(meta.relevance),
        })
        .collect();

    let resp = ListResponse { memories };
    (StatusCode::OK, axum::Json(resp))
}

async fn get_memory(
    State(state): State<AppState>,
    Path((namespace, key)): Path<(String, String)>,
) -> impl IntoResponse {
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "read", &namespace);
    state.metrics.record_read(&namespace);
    let internal_key = NamespacedKey::new(&namespace, &key).to_internal();

    match state.engine.get_with_meta(&internal_key) {
        Some((value, meta)) => {
            let resp = StoreResponse {
                key,
                value: String::from_utf8_lossy(&value).to_string(),
                r#type: Some(meta.memory_type.to_string()),
                ttl_ms: meta.ttl_remaining_ms,
                relevance: Some(meta.relevance),
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
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "delete", &namespace);
    let internal_key = NamespacedKey::new(&namespace, &key).to_internal();
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
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "delete", &namespace);
    let prefix = NamespacedKey::new(&namespace, "").prefix();
    let count = state.engine.delete_prefix(&prefix);

    // Record the namespace deletion in WAL for replication
    if let Some(ref repl) = state.repl_state {
        repl.record_delete_prefix(prefix.as_bytes());
    }

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
    let _timer =
        crate::metrics::RequestTimer::new(state.metrics.clone(), "batch_write", &namespace);
    let mut stored = 0usize;
    for item in &body.memories {
        let internal_key = NamespacedKey::new(&namespace, &item.key).to_internal();
        let memory_type = item.r#type.unwrap_or(MemoryType::Semantic);
        let value_bytes = item.value.as_bytes();
        let result = if item.embedding.is_some() {
            state.engine.set_with_embedding(
                &internal_key,
                value_bytes.to_vec(),
                item.ttl_ms,
                memory_type,
                item.embedding.clone(),
            )
        } else {
            state.engine.set(
                &internal_key,
                value_bytes.to_vec(),
                item.ttl_ms,
                memory_type,
            )
        };
        match result {
            Ok(()) => stored += 1,
            Err(ShardFullError {
                max_entries,
                current,
                shard_index,
            }) => {
                let body = serde_json::json!({
                    "error": "max entries exceeded",
                    "max_entries": max_entries,
                    "current": current,
                    "stored": stored,
                    "shard": shard_index,
                });
                return (StatusCode::INSUFFICIENT_STORAGE, axum::Json(body)).into_response();
            }
        }
    }

    let resp = BatchStoreResponse { ok: true, stored };
    state.metrics.record_write(&namespace);
    (StatusCode::CREATED, axum::Json(resp)).into_response()
}

async fn batch_get(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::Json(body): axum::Json<BatchGetRequest>,
) -> impl IntoResponse {
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "batch_read", &namespace);
    state.metrics.record_read(&namespace);
    let mut memories = Vec::with_capacity(body.keys.len());
    let mut missing = Vec::new();

    for key in &body.keys {
        let internal_key = NamespacedKey::new(&namespace, key).to_internal();
        match state.engine.get_with_meta(&internal_key) {
            Some((value, meta)) => {
                memories.push(StoreResponse {
                    key: key.clone(),
                    value: String::from_utf8_lossy(&value).to_string(),
                    r#type: Some(meta.memory_type.to_string()),
                    ttl_ms: meta.ttl_remaining_ms,
                    relevance: Some(meta.relevance),
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
    let _timer = crate::metrics::RequestTimer::new(state.metrics.clone(), "search", &namespace);
    state.metrics.record_read(&namespace);
    let results = state
        .engine
        .search_embedding(&namespace, &req.embedding, req.top_k);
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
            let start = std::time::Instant::now();
            match crate::store::persistence::snapshot_with_threshold(
                path,
                &state.engine,
                3,
                state.compression_threshold,
            ) {
                Ok(snapshot_path) => {
                    state.metrics.observe_snapshot_duration(start.elapsed());
                    state.metrics.set_snapshot_last_success();
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

// --- Share handlers ---

async fn grant_share(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::Json(req): axum::Json<GrantShareRequest>,
) -> impl IntoResponse {
    // Auth: calling token must own source_namespace
    let token = match extract_bearer_from_headers(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "Missing auth token"})),
            )
                .into_response();
        }
    };
    if !state.auth.is_authorized(&token, &req.source_namespace) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "Not authorized to share this namespace"})),
        )
            .into_response();
    }
    if req.source_namespace == req.target_namespace {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "Cannot share namespace with itself"})),
        )
            .into_response();
    }

    let policy = SharePolicy {
        source_namespace: req.source_namespace.clone(),
        target_namespace: req.target_namespace.clone(),
        permission: req.permission,
        key_prefix: req.key_prefix.clone(),
        created_at: now_ms(),
    };
    state.share.grant(policy);

    tracing::info!(
        source = %req.source_namespace,
        target = %req.target_namespace,
        permission = ?req.permission,
        prefix = ?req.key_prefix,
        "Share policy granted"
    );

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "granted"})),
    )
        .into_response()
}

async fn revoke_share(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::Json(req): axum::Json<RevokeShareRequest>,
) -> impl IntoResponse {
    let token = match extract_bearer_from_headers(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "Missing auth token"})),
            )
                .into_response();
        }
    };
    if !state.auth.is_authorized(&token, &req.source_namespace) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(
                serde_json::json!({"error": "Not authorized to revoke sharing for this namespace"}),
            ),
        )
            .into_response();
    }

    let removed = state.share.revoke(
        &req.source_namespace,
        &req.target_namespace,
        req.key_prefix.as_deref(),
    );

    if removed {
        tracing::info!(
            source = %req.source_namespace,
            target = %req.target_namespace,
            "Share policy revoked"
        );
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "revoked"})),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "No matching share policy found"})),
        )
            .into_response()
    }
}

async fn list_granted(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let namespace = match params.get("namespace") {
        Some(ns) => ns.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "Missing namespace parameter"})),
            )
                .into_response();
        }
    };
    let token = match extract_bearer_from_headers(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "Missing auth token"})),
            )
                .into_response();
        }
    };
    if !state.auth.is_authorized(&token, &namespace) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "Not authorized"})),
        )
            .into_response();
    }

    let policies = state.share.policies_for(&namespace);
    (StatusCode::OK, axum::Json(ShareListResponse { policies })).into_response()
}

async fn list_shared_with_me(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let namespace = match params.get("namespace") {
        Some(ns) => ns.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "Missing namespace parameter"})),
            )
                .into_response();
        }
    };
    let token = match extract_bearer_from_headers(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "Missing auth token"})),
            )
                .into_response();
        }
    };
    if !state.auth.is_authorized(&token, &namespace) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "Not authorized"})),
        )
            .into_response();
    }

    let policies = state.share.shared_with(&namespace);
    (StatusCode::OK, axum::Json(ShareListResponse { policies })).into_response()
}

/// Extract bearer token from HeaderMap (for use in handlers where we have HeaderMap, not Request).
fn extract_bearer_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

// --- Trigger Handlers ---

async fn register_trigger(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<RegisterTriggerRequest>,
) -> impl IntoResponse {
    let trigger = crate::store::trigger::trigger_from_config(
        req.id.clone(),
        req.condition,
        req.action,
        req.cooldown_ms,
    );
    match state.engine.trigger_manager().register(trigger) {
        Ok(()) => {
            let info = state.engine.trigger_manager().get(&req.id).unwrap();
            (StatusCode::OK, axum::Json(TriggerInfoResponse::from(info))).into_response()
        }
        Err(e) => (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn list_triggers(State(state): State<AppState>) -> impl IntoResponse {
    let triggers: Vec<TriggerInfoResponse> = state
        .engine
        .trigger_manager()
        .list()
        .into_iter()
        .map(TriggerInfoResponse::from)
        .collect();
    (StatusCode::OK, axum::Json(TriggerListResponse { triggers })).into_response()
}

async fn delete_trigger(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.engine.trigger_manager().unregister(&id) {
        (
            StatusCode::OK,
            axum::Json(SimpleResponse {
                ok: true,
                deleted: None,
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "trigger not found"})),
        )
            .into_response()
    }
}

async fn fire_trigger(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let mgr = state.engine.trigger_manager();
    let info = match mgr.get(&id) {
        Some(i) => i,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"error": "trigger not found"})),
            )
                .into_response();
        }
    };

    let now_ms = now_ms();
    match state
        .engine
        .check_trigger_condition(&info.condition, now_ms)
    {
        Some(entries) => {
            let count = entries.len();
            if let Some(ctx) = mgr.force_fire(&id, entries, now_ms) {
                // Execute the action if configured
                if let Some(config) = mgr.get_action_config(&id) {
                    tokio::spawn(async move {
                        if let Err(e) = crate::store::trigger::execute_webhook(&config, ctx).await {
                            tracing::warn!("trigger webhook failed: id={}, error={}", id, e);
                        }
                    });
                    (
                        StatusCode::OK,
                        axum::Json(TriggerFireResponse {
                            ok: true,
                            matching_entries: count,
                            message: Some("trigger fired, webhook dispatched".to_string()),
                        }),
                    )
                        .into_response()
                } else {
                    (
                        StatusCode::OK,
                        axum::Json(TriggerFireResponse {
                            ok: true,
                            matching_entries: count,
                            message: Some(
                                "trigger fired, no action_config set (in-process only)".to_string(),
                            ),
                        }),
                    )
                        .into_response()
                }
            } else {
                (
                    StatusCode::OK,
                    axum::Json(TriggerFireResponse {
                        ok: false,
                        matching_entries: count,
                        message: Some("trigger is disabled".to_string()),
                    }),
                )
                    .into_response()
            }
        }
        None => (
            StatusCode::OK,
            axum::Json(TriggerFireResponse {
                ok: false,
                matching_entries: 0,
                message: Some("condition not met, no matching entries".to_string()),
            }),
        )
            .into_response(),
    }
}

async fn enable_trigger(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.engine.trigger_manager().set_enabled(&id, true) {
        let info = state.engine.trigger_manager().get(&id).unwrap();
        (StatusCode::OK, axum::Json(TriggerInfoResponse::from(info))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "trigger not found"})),
        )
            .into_response()
    }
}

async fn disable_trigger(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.engine.trigger_manager().set_enabled(&id, false) {
        let info = state.engine.trigger_manager().get(&id).unwrap();
        (StatusCode::OK, axum::Json(TriggerInfoResponse::from(info))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "trigger not found"})),
        )
            .into_response()
    }
}

// --- Consolidation handlers ---

async fn consolidate(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Query(query): Query<ConsolidateQuery>,
) -> impl IntoResponse {
    let mgr = state.engine.consolidation_manager();
    if !mgr.is_enabled() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "consolidation is disabled"})),
        )
            .into_response();
    }

    let rules = mgr.rules();
    let filtered: Vec<_> = match query.rule_type.as_deref() {
        Some("promote") => rules
            .into_iter()
            .filter(|r| matches!(r, ConsolidationRule::Promote { .. }))
            .collect(),
        Some("compress") => rules
            .into_iter()
            .filter(|r| matches!(r, ConsolidationRule::Compress { .. }))
            .collect(),
        _ => rules,
    };

    let result =
        crate::store::consolidation::run_consolidation(&state.engine, &namespace, &filtered);

    // Update metadata
    let mut meta = mgr.get_meta(&namespace).unwrap_or_default();
    meta.last_run_ms = crate::time::now_ms();
    meta.total_promoted += result.promoted as u64;
    meta.total_compressed += result.compressed as u64;
    mgr.update_meta(&namespace, meta);

    let response: ConsolidateResponse = result.into();
    (StatusCode::OK, axum::Json(serde_json::json!(response))).into_response()
}

async fn consolidation_status(State(state): State<AppState>) -> impl IntoResponse {
    let mgr = state.engine.consolidation_manager();
    axum::Json(ConsolidationStatusResponse {
        enabled: mgr.is_enabled(),
        rules_count: mgr.rules().len(),
        interval_ms: state.consolidation_interval_ms,
    })
}
