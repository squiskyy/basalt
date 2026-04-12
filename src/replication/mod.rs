pub mod wal;
pub mod primary;
pub mod replica;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;
use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;
use crate::time::now_ms;

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
}

impl ReplicationState {
    /// Create a new ReplicationState in Primary role with the given WAL size.
    pub fn new_primary(engine: Arc<KvEngine>, wal_size: usize) -> Self {
        ReplicationState {
            role: std::sync::Mutex::new(ReplicationRole::Primary),
            wal: Arc::new(wal::Wal::new(wal_size)),
            connected_replicas: AtomicU64::new(0),
            replication_offset: AtomicU64::new(0),
            replica_shutdown_tx: std::sync::Mutex::new(None),
            engine,
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

    /// Record a SET write in the WAL and update offset.
    /// Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
    /// When ttl_ms is None, we store 0 in the WalEntry.
    pub fn record_set(&self, key: &[u8], value: &[u8], mem_type: MemoryType, ttl_ms: Option<u64>) {
        let entry = wal::WalEntry {
            op: wal::WalOp::Set,
            key: key.to_vec(),
            value: value.to_vec(),
            mem_type,
            ttl_ms: ttl_ms.unwrap_or(0),
            timestamp_ms: now_ms(),
            embedding: None,
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
        match &*role {
            ReplicationRole::Primary => {
                format!(
                    "# Replication\r\nrole:primary\r\nconnected_replicas:{}\r\nreplication_offset:{}\r\nwal_size:{}\r\n",
                    self.connected_replicas(),
                    self.replication_offset(),
                    self.wal.len(),
                )
            }
            ReplicationRole::Replica { primary_host, primary_port } => {
                format!(
                    "# Replication\r\nrole:replica\r\nprimary_host:{}\r\nprimary_port:{}\r\nreplication_offset:{}\r\n",
                    primary_host,
                    primary_port,
                    self.replication_offset(),
                )
            }
        }
    }
}
