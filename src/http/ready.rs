use std::sync::atomic::{AtomicBool, Ordering};

/// Tracks whether the server is ready to serve traffic.
///
/// Readiness depends on:
/// - Engine initialization (always true after construction)
/// - Snapshot restore completion
/// - Replica sync completion (if configured as a replica)
///
/// This is shared via `Arc` between the main task (which sets ready state
/// after restore/sync) and the HTTP handler (which reads it on `/ready`).
pub struct ReadyState {
    /// True when the server is fully ready to accept traffic.
    ready: AtomicBool,
    /// Human-readable reason when not ready (e.g., "restoring_snapshot", "replica_syncing").
    /// Empty when ready.
    reason: std::sync::Mutex<String>,
}

impl ReadyState {
    /// Create a new ReadyState. Starts as not-ready with the given reason.
    pub fn new(initial_reason: &str) -> Self {
        ReadyState {
            ready: AtomicBool::new(false),
            reason: std::sync::Mutex::new(initial_reason.to_string()),
        }
    }

    /// Create a ReadyState that is immediately ready.
    /// Used when there's no snapshot restore or replication to wait for.
    pub fn new_ready() -> Self {
        ReadyState {
            ready: AtomicBool::new(true),
            reason: std::sync::Mutex::new(String::new()),
        }
    }

    /// Mark the server as ready to serve traffic.
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
        if let Ok(mut reason) = self.reason.lock() {
            reason.clear();
        }
    }

    /// Mark the server as not ready with a specific reason.
    pub fn set_not_ready(&self, reason: &str) {
        self.ready.store(false, Ordering::Release);
        if let Ok(mut r) = self.reason.lock() {
            *r = reason.to_string();
        }
    }

    /// Check if the server is ready.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Get the current not-ready reason. Returns empty string if ready.
    pub fn reason(&self) -> String {
        match self.reason.lock() {
            Ok(r) => r.clone(),
            Err(_) => String::new(),
        }
    }
}

/// Readiness check response (serialized to JSON).
#[derive(serde::Serialize)]
pub struct ReadyResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Health check response (for `/health`, always OK when process is alive).
#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub status: String,
}
