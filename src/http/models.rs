use crate::store::memory_type::MemoryType;
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

/// Health check response.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

/// Server info response.
#[derive(Debug, Serialize, Deserialize)]
pub struct InfoResponse {
    pub version: String,
    pub shards: usize,
    pub compression_threshold: usize,
}

/// Query parameters for list endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListQuery {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<MemoryType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}
