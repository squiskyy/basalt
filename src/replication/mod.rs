pub mod primary;
pub mod replica;
pub mod wal;

use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;
use crate::time::now_ms;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

/// Replication role of this node.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplicationRole {
    /// This node is a primary and accepts replica connections.
    Primary,
    /// This node is replicating from a primary.
    Replica {
        primary_host: String,
        primary_port: u16,
    },
}

/// Failover configuration for automatic promotion on primary failure.
#[derive(Debug, Clone)]
pub struct FailoverConfig {
    /// Whether automatic failover is enabled.
    pub enabled: bool,
    /// How long (ms) without a lease heartbeat before considering the primary dead.
    pub timeout_ms: u64,
    /// Known peer addresses for reconfiguration after failover.
    pub peers: Vec<String>,
}

impl FailoverConfig {
    pub fn new(enabled: bool, timeout_ms: u64, peers: Vec<String>) -> Self {
        Self {
            enabled,
            timeout_ms,
            peers,
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            timeout_ms: 15_000,
            peers: Vec::new(),
        }
    }
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Global replication state shared across the server.
pub struct ReplicationState {
    /// Current role.
    role: std::sync::Mutex<ReplicationRole>,
    /// The WAL (only used when role is Primary).
    wal: Arc<wal::Wal>,
    /// Connected replica count (primary side).
    connected_replicas: AtomicU64,
    /// Current replication offset (sequence number of the last WAL entry).
    replication_offset: AtomicU64,
    /// Shutdown signal for replica stream task.
    replica_shutdown_tx: std::sync::Mutex<Option<watch::Sender<bool>>>,
    /// Reference to the KV engine.
    engine: Arc<KvEngine>,
    /// Unique node ID for this instance (random hex string).
    node_id: String,
    /// Monotonically increasing epoch for failover fencing.
    /// Starts at 0, incremented on each promotion.
    current_epoch: AtomicU64,
    /// Timestamp (ms since epoch) of the last LEASE received from the primary.
    /// 0 means no lease received yet.
    last_lease_ms: AtomicU64,
    /// The epoch of the primary we are currently following (replica side).
    primary_epoch: AtomicU64,
    /// Failover configuration.
    failover_config: std::sync::Mutex<FailoverConfig>,
}

impl ReplicationState {
    /// Create a new ReplicationState in Primary role with the given WAL size.
    pub fn new_primary(engine: Arc<KvEngine>, wal_size: usize) -> Self {
        let node_id = format!("{:016x}", rand::random::<u64>());
        ReplicationState {
            role: std::sync::Mutex::new(ReplicationRole::Primary),
            wal: Arc::new(wal::Wal::new(wal_size)),
            connected_replicas: AtomicU64::new(0),
            replication_offset: AtomicU64::new(0),
            replica_shutdown_tx: std::sync::Mutex::new(None),
            engine,
            node_id,
            current_epoch: AtomicU64::new(0),
            last_lease_ms: AtomicU64::new(0),
            primary_epoch: AtomicU64::new(0),
            failover_config: std::sync::Mutex::new(FailoverConfig::disabled()),
        }
    }

    /// Get the current role.
    pub fn role(&self) -> ReplicationRole {
        self.role.lock().unwrap().clone()
    }

    /// Set the role.
    pub fn set_role(&self, new_role: ReplicationRole) {
        *self.role.lock().unwrap() = new_role;
    }

    /// Get a reference to the WAL.
    pub fn wal(&self) -> &Arc<wal::Wal> {
        &self.wal
    }

    /// Get the connected replica count.
    pub fn connected_replicas(&self) -> u64 {
        self.connected_replicas.load(Ordering::Relaxed)
    }

    /// Increment connected replica count.
    pub fn inc_connected_replicas(&self) {
        self.connected_replicas.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement connected replica count.
    pub fn dec_connected_replicas(&self) {
        self.connected_replicas.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get the replication offset.
    pub fn replication_offset(&self) -> u64 {
        self.replication_offset.load(Ordering::Relaxed)
    }

    /// Set the replication offset.
    pub fn set_replication_offset(&self, offset: u64) {
        self.replication_offset.store(offset, Ordering::Relaxed);
    }

    /// Get a reference to the engine.
    pub fn engine(&self) -> &Arc<KvEngine> {
        &self.engine
    }

    /// Get this node's unique ID.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Get the current epoch.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch.load(Ordering::Relaxed)
    }

    /// Set the epoch (used when learning about a higher epoch from another node).
    pub fn set_epoch(&self, epoch: u64) {
        self.current_epoch.store(epoch, Ordering::Relaxed);
    }

    /// Increment the epoch atomically and return the new value.
    /// Used during failover promotion.
    pub fn increment_epoch(&self) -> u64 {
        self.current_epoch.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Get the timestamp of the last LEASE received from the primary (ms since epoch).
    /// Returns 0 if no lease has been received yet.
    pub fn last_lease_ms(&self) -> u64 {
        self.last_lease_ms.load(Ordering::Relaxed)
    }

    /// Set the last lease timestamp (called when a LEASE is received from the primary).
    pub fn set_last_lease_ms(&self, ts: u64) {
        self.last_lease_ms.store(ts, Ordering::Relaxed);
    }

    /// Get the epoch of the primary we are following.
    pub fn primary_epoch(&self) -> u64 {
        self.primary_epoch.load(Ordering::Relaxed)
    }

    /// Set the primary epoch (called when a LEASE is received from the primary).
    pub fn set_primary_epoch(&self, epoch: u64) {
        self.primary_epoch.store(epoch, Ordering::Relaxed);
    }

    /// Check if the primary lease has expired.
    /// Returns true if the last lease was more than timeout_ms ago.
    /// Returns false if no lease has been received yet (last_lease_ms == 0).
    pub fn is_lease_expired(&self, now_ms: u64, timeout_ms: u64) -> bool {
        let last = self.last_lease_ms();
        if last == 0 {
            return false; // never received a lease yet
        }
        now_ms.saturating_sub(last) > timeout_ms
    }

    /// Promote this replica to primary. Increments epoch, changes role.
    /// Returns the new epoch.
    pub fn promote_to_primary(&self) -> u64 {
        let new_epoch = self.primary_epoch() + 1;
        self.set_epoch(new_epoch);
        self.set_role(ReplicationRole::Primary);
        self.set_last_lease_ms(0); // clear lease tracking
        tracing::info!("promoted to primary with epoch {}", new_epoch);
        new_epoch
    }

    /// Demote this node from primary to replica following the given primary.
    /// Used when a stale primary discovers it has been replaced.
    pub fn demote_to_replica(&self, primary_host: String, primary_port: u16, new_epoch: u64) {
        tracing::warn!(
            "demoting to replica of {}:{}, new_epoch={} (was epoch={})",
            primary_host,
            primary_port,
            new_epoch,
            self.current_epoch()
        );
        self.set_epoch(new_epoch);
        self.set_role(ReplicationRole::Replica {
            primary_host,
            primary_port,
        });
    }

    /// Set the failover configuration.
    pub fn set_failover_config(&self, config: FailoverConfig) {
        *self.failover_config.lock().unwrap() = config;
    }

    /// Get the failover configuration.
    pub fn failover_config(&self) -> FailoverConfig {
        self.failover_config.lock().unwrap().clone()
    }

    /// Record a SET write in the WAL and update offset.
    /// Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
    /// When ttl_ms is None, we store 0 in the WalEntry.
    pub fn record_set(&self, key: &[u8], value: &[u8], mem_type: MemoryType, ttl_ms: Option<u64>) {
        self.record_set_with_embedding(key, value, mem_type, ttl_ms, None);
    }

    /// Record a SET write with an optional embedding vector in the WAL.
    pub fn record_set_with_embedding(
        &self,
        key: &[u8],
        value: &[u8],
        mem_type: MemoryType,
        ttl_ms: Option<u64>,
        embedding: Option<Vec<f32>>,
    ) {
        let entry = wal::WalEntry {
            op: wal::WalOp::Set,
            key: key.to_vec(),
            value: value.to_vec(),
            mem_type,
            ttl_ms: ttl_ms.unwrap_or(0),
            timestamp_ms: now_ms(),
            embedding,
        };
        let seq = self.wal.append(entry);
        self.replication_offset.store(seq, Ordering::Relaxed);
    }

    /// Record a DELETE write in the WAL and update offset.
    pub fn record_delete(&self, key: &[u8]) {
        let entry = wal::WalEntry {
            op: wal::WalOp::Delete,
            key: key.to_vec(),
            value: Vec::new(),
            mem_type: MemoryType::Semantic, // doesn't matter for delete
            ttl_ms: 0,
            timestamp_ms: now_ms(),
            embedding: None,
        };
        let seq = self.wal.append(entry);
        self.replication_offset.store(seq, Ordering::Relaxed);
    }

    /// Record a DELETE_PREFIX write in the WAL and update offset.
    pub fn record_delete_prefix(&self, prefix: &[u8]) {
        let entry = wal::WalEntry {
            op: wal::WalOp::DeletePrefix,
            key: prefix.to_vec(),
            value: Vec::new(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 0,
            timestamp_ms: now_ms(),
            embedding: None,
        };
        let seq = self.wal.append(entry);
        self.replication_offset.store(seq, Ordering::Relaxed);
    }

    /// Set the replica shutdown sender.
    pub fn set_replica_shutdown(&self, tx: Option<watch::Sender<bool>>) {
        *self.replica_shutdown_tx.lock().unwrap() = tx;
    }

    /// Signal the replica stream to stop.
    pub fn stop_replica_stream(&self) {
        if let Some(tx) = self.replica_shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(true);
        }
    }

    /// Build an INFO replication string.
    pub fn info_string(&self) -> String {
        let role = self.role.lock().unwrap();
        let failover = self.failover_config();
        match &*role {
            ReplicationRole::Primary => {
                format!(
                    "# Replication\r\nrole:primary\r\nnode_id:{}\r\nepoch:{}\r\nconnected_replicas:{}\r\nreplication_offset:{}\r\nwal_size:{}\r\nfailover:{}\r\nfailover_timeout_ms:{}\r\n",
                    self.node_id,
                    self.current_epoch(),
                    self.connected_replicas(),
                    self.replication_offset(),
                    self.wal.len(),
                    if failover.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    failover.timeout_ms,
                )
            }
            ReplicationRole::Replica {
                primary_host,
                primary_port,
            } => {
                format!(
                    "# Replication\r\nrole:replica\r\nnode_id:{}\r\nprimary_host:{}\r\nprimary_port:{}\r\nprimary_epoch:{}\r\nreplication_offset:{}\r\nlast_lease_ms:{}\r\nfailover:{}\r\nfailover_timeout_ms:{}\r\n",
                    self.node_id,
                    primary_host,
                    primary_port,
                    self.primary_epoch(),
                    self.replication_offset(),
                    self.last_lease_ms(),
                    if failover.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    failover.timeout_ms,
                )
            }
        }
    }
}

/// Background task that monitors the primary lease and triggers automatic
/// failover if the primary becomes unreachable for longer than the configured
/// timeout. Only active when failover is enabled and the node is a replica.
pub async fn failover_monitor(
    repl_state: Arc<ReplicationState>,
    failover_config: FailoverConfig,
    ready_state: Arc<crate::http::ready::ReadyState>,
    _engine: Arc<KvEngine>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    if !failover_config.enabled {
        return;
    }

    // Check at 1/3 of the timeout interval to detect expiry quickly
    let check_interval = tokio::time::Duration::from_millis(failover_config.timeout_ms / 3);
    let mut interval = tokio::time::interval(check_interval);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Only monitor when we're a replica
                if !matches!(repl_state.role(), ReplicationRole::Replica { .. }) {
                    continue;
                }

                let now = crate::time::now_ms();
                if repl_state.is_lease_expired(now, failover_config.timeout_ms) {
                    let last = repl_state.last_lease_ms();
                    tracing::warn!(
                        "primary lease expired (last lease {}ms ago, timeout {}ms) - triggering failover",
                        now.saturating_sub(last),
                        failover_config.timeout_ms
                    );

                    // Signal not-ready during failover transition
                    ready_state.set_not_ready("failover_in_progress");

                    // Stop the replica stream
                    repl_state.stop_replica_stream();

                    // Promote to primary
                    let new_epoch = repl_state.promote_to_primary();
                    tracing::info!("failover: promoted to primary, epoch={new_epoch}");

                    // Signal ready again (we're now accepting writes as primary)
                    ready_state.set_ready();
                }
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("failover monitor shutting down");
                return;
            }
        }
    }
}
