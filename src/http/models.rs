use crate::store::memory_type::MemoryType;
use crate::store::share::SharePolicy;
use serde::{Deserialize, Serialize};

/// Request body for storing a single memory entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreRequest {
    pub key: String,
    pub value: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<MemoryType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// Optional embedding vector for semantic similarity search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// Request body for batch storing multiple memories.
#[derive(Debug, Serialize, Deserialize)]
pub struct BatchStoreRequest {
    pub memories: Vec<StoreRequest>,
}

/// Response body for a single memory entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreResponse {
    pub key: String,
    pub value: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevance: Option<f64>,
}

/// Request body for batch retrieving multiple keys.
#[derive(Debug, Serialize, Deserialize)]
pub struct BatchGetRequest {
    pub keys: Vec<String>,
}

/// Response body for batch store operation.
#[derive(Debug, Serialize, Deserialize)]
pub struct BatchStoreResponse {
    pub ok: bool,
    pub stored: usize,
}

/// Response body for batch get operation.
#[derive(Debug, Serialize, Deserialize)]
pub struct BatchGetResponse {
    pub memories: Vec<StoreResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<Vec<String>>,
}

/// Simple acknowledgement response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SimpleResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<usize>,
}

/// Response body for listing memories.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub memories: Vec<StoreResponse>,
}

/// Server info response.
#[derive(Debug, Serialize, Deserialize)]
pub struct InfoResponse {
    pub version: String,
    pub shards: usize,
    pub compression_threshold: usize,
    pub eviction_policy: String,
    pub max_entries_per_shard: usize,
    pub shard_entries: Vec<usize>,
}

/// Query parameters for list endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListQuery {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<MemoryType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,
}

/// Request body for vector similarity search.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The query embedding vector.
    pub embedding: Vec<f32>,
    /// Number of top results to return. Defaults to 10.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize {
    10
}

/// A single result from a vector similarity search.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub key: String,
    /// Cosine distance (0 = identical, 2 = opposite). Lower is more similar.
    pub distance: f32,
    pub value: String,
}

/// Response body for vector similarity search.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

// --- Share Models ---

/// Request body for granting a sharing policy.
#[derive(Debug, Deserialize)]
pub struct GrantShareRequest {
    pub source_namespace: String,
    pub target_namespace: String,
    pub permission: crate::store::share::SharePermission,
    pub key_prefix: Option<String>,
}

/// Request body for revoking a sharing policy.
#[derive(Debug, Deserialize)]
pub struct RevokeShareRequest {
    pub source_namespace: String,
    pub target_namespace: String,
    pub key_prefix: Option<String>,
}

/// Response body for listing sharing policies.
#[derive(Debug, Serialize)]
pub struct ShareListResponse {
    pub policies: Vec<SharePolicy>,
}

// --- Trigger Models ---

/// Request body for registering a summarization trigger.
#[derive(Debug, Deserialize)]
pub struct RegisterTriggerRequest {
    pub id: String,
    pub condition: crate::store::trigger::TriggerCondition,
    /// Optional action configuration (webhook URL or log).
    /// If omitted, the trigger only fires in-process callbacks.
    #[serde(default)]
    pub action: Option<crate::store::trigger::TriggerActionConfig>,
    /// Minimum time between fires in milliseconds. Defaults to 60000 (1 minute).
    #[serde(default = "default_trigger_cooldown_ms")]
    pub cooldown_ms: u64,
}

fn default_trigger_cooldown_ms() -> u64 {
    60_000
}

/// Response body for trigger info.
#[derive(Debug, Serialize)]
pub struct TriggerInfoResponse {
    pub id: String,
    pub condition: crate::store::trigger::TriggerCondition,
    pub action: Option<crate::store::trigger::TriggerActionConfig>,
    pub enabled: bool,
    pub cooldown_ms: u64,
    pub last_fired_ms: Option<u64>,
}

impl From<crate::store::trigger::TriggerInfo> for TriggerInfoResponse {
    fn from(info: crate::store::trigger::TriggerInfo) -> Self {
        TriggerInfoResponse {
            id: info.id,
            condition: info.condition,
            action: info.action_config,
            enabled: info.enabled,
            cooldown_ms: info.cooldown_ms,
            last_fired_ms: info.last_fired_ms,
        }
    }
}

/// Response body for listing triggers.
#[derive(Debug, Serialize)]
pub struct TriggerListResponse {
    pub triggers: Vec<TriggerInfoResponse>,
}

/// Response body for manual trigger fire.
#[derive(Debug, Serialize)]
pub struct TriggerFireResponse {
    pub ok: bool,
    /// Number of matching entries when the trigger fired.
    pub matching_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// --- Consolidation Models ---

/// Query parameters for the consolidate endpoint.
#[derive(Debug, Deserialize)]
pub struct ConsolidateQuery {
    /// Only run rules of this type ("promote" or "compress"). Optional.
    #[serde(rename = "type")]
    pub rule_type: Option<String>,
}

/// Response body for consolidation.
#[derive(Debug, Serialize)]
pub struct ConsolidateResponse {
    pub promoted: usize,
    pub compressed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub details: Vec<ConsolidateDetailResponse>,
}

/// A single consolidation action detail in the response.
#[derive(Debug, Serialize)]
pub struct ConsolidateDetailResponse {
    pub action: String,
    pub from: String,
    pub to: String,
    pub occurrences: usize,
}

impl From<crate::store::consolidation::ConsolidationResult> for ConsolidateResponse {
    fn from(r: crate::store::consolidation::ConsolidationResult) -> Self {
        ConsolidateResponse {
            promoted: r.promoted,
            compressed: r.compressed,
            skipped: r.skipped,
            failed: r.failed,
            details: r
                .details
                .into_iter()
                .map(|d| ConsolidateDetailResponse {
                    action: d.action,
                    from: d.from,
                    to: d.to,
                    occurrences: d.occurrences,
                })
                .collect(),
        }
    }
}

/// Response body for consolidation status.
#[derive(Debug, Serialize)]
pub struct ConsolidationStatusResponse {
    pub enabled: bool,
    pub rules_count: usize,
    pub interval_ms: u64,
}
