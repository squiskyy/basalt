use basalt::replication::wal::{Wal, WalEntry, WalOp, deserialize_entry, serialize_entry};
use basalt::replication::{ReplicationRole, ReplicationState};
use basalt::store::memory_type::MemoryType;
use basalt::store::{ConsolidationManager, KvEngine};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// WAL unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_wal_append_and_read() {
    let wal = Wal::new(100);

    let e1 = WalEntry {
        op: WalOp::Set,
        key: b"key1".to_vec(),
        value: b"val1".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 100,
        embedding: None,
    };
    let e2 = WalEntry {
        op: WalOp::Set,
        key: b"key2".to_vec(),
        value: b"val2".to_vec(),
        mem_type: MemoryType::Episodic,
        ttl_ms: 5000,
        timestamp_ms: 200,
        embedding: None,
    };

    let seq1 = wal.append(e1);
    let seq2 = wal.append(e2);
    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    // Read all entries from the beginning
    let entries = wal.entries_from(1);
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].0, 1);
    assert_eq!(entries[0].1.op, WalOp::Set);
    assert_eq!(entries[0].1.key, b"key1");
    assert_eq!(entries[0].1.value, b"val1");
    assert_eq!(entries[0].1.mem_type, MemoryType::Semantic);
    assert_eq!(entries[0].1.ttl_ms, 0);

    assert_eq!(entries[1].0, 2);
    assert_eq!(entries[1].1.key, b"key2");
    assert_eq!(entries[1].1.value, b"val2");
    assert_eq!(entries[1].1.mem_type, MemoryType::Episodic);
    assert_eq!(entries[1].1.ttl_ms, 5000);

    // Read only from seq 2
    let from_2 = wal.entries_from(2);
    assert_eq!(from_2.len(), 1);
    assert_eq!(from_2[0].0, 2);
}

#[test]
fn test_wal_circular_buffer() {
    let max_size = 3;
    let wal = Wal::new(max_size);

    // Append 5 entries to a WAL with max_size=3
    for i in 0..5u64 {
        wal.append(WalEntry {
            op: WalOp::Set,
            key: format!("key{i}").into_bytes(),
            value: b"v".to_vec(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 0,
            timestamp_ms: i,
            embedding: None,
        });
    }

    // Only the last 3 entries should be retained (seqs 3, 4, 5)
    assert_eq!(wal.len(), 3);

    // entries_from(1) should only return the available entries (3, 4, 5)
    let all_available = wal.entries_from(1);
    assert_eq!(all_available.len(), 3);
    assert_eq!(all_available[0].0, 3);
    assert_eq!(all_available[1].0, 4);
    assert_eq!(all_available[2].0, 5);

    // entries_from(4) should return only seqs 4, 5
    let from_4 = wal.entries_from(4);
    assert_eq!(from_4.len(), 2);
    assert_eq!(from_4[0].0, 4);
    assert_eq!(from_4[1].0, 5);

    // entries_from(10) should return nothing (all evicted or not yet assigned)
    let from_10 = wal.entries_from(10);
    assert!(from_10.is_empty());

    // oldest_seq should reflect the oldest retained entry
    assert_eq!(wal.oldest_seq(), Some(3));
}

#[test]
fn test_wal_serialize_deserialize() {
    let entry = WalEntry {
        op: WalOp::Set,
        key: b"hello".to_vec(),
        value: b"world".to_vec(),
        mem_type: MemoryType::Episodic,
        ttl_ms: 30000,
        timestamp_ms: 1234567890,
        embedding: None,
    };

    let data = serialize_entry(&entry, 42);
    let (seq, decoded, consumed) = deserialize_entry(&data).unwrap();

    assert_eq!(seq, 42);
    assert_eq!(consumed, data.len());
    assert_eq!(decoded.op, WalOp::Set);
    assert_eq!(decoded.key, b"hello");
    assert_eq!(decoded.value, b"world");
    assert_eq!(decoded.mem_type, MemoryType::Episodic);
    assert_eq!(decoded.ttl_ms, 30000);
    assert_eq!(decoded.timestamp_ms, 1234567890);
}

#[test]
fn test_wal_empty() {
    let wal = Wal::new(100);
    assert_eq!(wal.len(), 0);
    assert!(wal.is_empty());

    // After appending one entry, it should no longer be empty
    wal.append(WalEntry {
        op: WalOp::Set,
        key: b"k".to_vec(),
        value: b"v".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 0,
        embedding: None,
    });
    assert_eq!(wal.len(), 1);
    assert!(!wal.is_empty());
}

#[test]
fn test_wal_sequence_numbers() {
    let wal = Wal::new(100);

    // New WAL starts with next_seq = 1
    assert_eq!(wal.current_seq(), 1);

    let seq1 = wal.append(WalEntry {
        op: WalOp::Set,
        key: b"a".to_vec(),
        value: b"1".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 0,
        embedding: None,
    });
    assert_eq!(seq1, 1);
    assert_eq!(wal.current_seq(), 2);

    let seq2 = wal.append(WalEntry {
        op: WalOp::Set,
        key: b"b".to_vec(),
        value: b"2".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 0,
        embedding: None,
    });
    assert_eq!(seq2, 2);
    assert_eq!(wal.current_seq(), 3);

    let seq3 = wal.append(WalEntry {
        op: WalOp::Delete,
        key: b"a".to_vec(),
        value: Vec::new(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 0,
        embedding: None,
    });
    assert_eq!(seq3, 3);
    assert_eq!(wal.current_seq(), 4);
}

// ---------------------------------------------------------------------------
// ReplicationState unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_replication_state_primary() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // New primary state should have Primary role
    assert_eq!(state.role(), ReplicationRole::Primary);

    // Defaults: no connected replicas, zero offset
    assert_eq!(state.connected_replicas(), 0);
    assert_eq!(state.replication_offset(), 0);

    // WAL should be empty
    assert!(state.wal().is_empty());
}

#[test]
fn test_replication_state_record_set() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    state.record_set(b"mykey", b"myval", MemoryType::Semantic, Some(5000));

    // WAL should have one entry
    assert_eq!(state.wal().len(), 1);

    // Replication offset should be updated to seq 1
    assert_eq!(state.replication_offset(), 1);

    // Verify the entry content
    let entries = state.wal().entries_from(1);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0].1;
    assert_eq!(entry.op, WalOp::Set);
    assert_eq!(entry.key, b"mykey");
    assert_eq!(entry.value, b"myval");
    assert_eq!(entry.mem_type, MemoryType::Semantic);
    assert_eq!(entry.ttl_ms, 5000);
}

#[test]
fn test_replication_state_record_delete() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    state.record_delete(b"mykey");

    // WAL should have one entry
    assert_eq!(state.wal().len(), 1);

    // Replication offset should be updated to seq 1
    assert_eq!(state.replication_offset(), 1);

    // Verify the entry content
    let entries = state.wal().entries_from(1);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0].1;
    assert_eq!(entry.op, WalOp::Delete);
    assert_eq!(entry.key, b"mykey");
    assert!(entry.value.is_empty());
    assert_eq!(entry.ttl_ms, 0);
}

// ---------------------------------------------------------------------------
// WAL serialization round-trip for all 3 op types
// ---------------------------------------------------------------------------

#[test]
fn test_wal_serialize_roundtrip_all_ops() {
    // SET
    let set_entry = WalEntry {
        op: WalOp::Set,
        key: b"set_key".to_vec(),
        value: b"set_value".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 9999,
        timestamp_ms: 111111,
        embedding: None,
    };
    let set_data = serialize_entry(&set_entry, 1);
    let (seq, decoded, consumed) = deserialize_entry(&set_data).unwrap();
    assert_eq!(seq, 1);
    assert_eq!(consumed, set_data.len());
    assert_eq!(decoded.op, WalOp::Set);
    assert_eq!(decoded.key, b"set_key");
    assert_eq!(decoded.value, b"set_value");
    assert_eq!(decoded.mem_type, MemoryType::Semantic);
    assert_eq!(decoded.ttl_ms, 9999);
    assert_eq!(decoded.timestamp_ms, 111111);

    // DELETE
    let del_entry = WalEntry {
        op: WalOp::Delete,
        key: b"del_key".to_vec(),
        value: Vec::new(),
        mem_type: MemoryType::Episodic,
        ttl_ms: 0,
        timestamp_ms: 222222,
        embedding: None,
    };
    let del_data = serialize_entry(&del_entry, 2);
    let (seq, decoded, consumed) = deserialize_entry(&del_data).unwrap();
    assert_eq!(seq, 2);
    assert_eq!(consumed, del_data.len());
    assert_eq!(decoded.op, WalOp::Delete);
    assert_eq!(decoded.key, b"del_key");
    assert!(decoded.value.is_empty());
    assert_eq!(decoded.mem_type, MemoryType::Episodic);
    assert_eq!(decoded.ttl_ms, 0);
    assert_eq!(decoded.timestamp_ms, 222222);

    // DELETE_PREFIX
    let dpx_entry = WalEntry {
        op: WalOp::DeletePrefix,
        key: b"ns:".to_vec(),
        value: Vec::new(),
        mem_type: MemoryType::Procedural,
        ttl_ms: 0,
        timestamp_ms: 333333,
        embedding: None,
    };
    let dpx_data = serialize_entry(&dpx_entry, 3);
    let (seq, decoded, consumed) = deserialize_entry(&dpx_data).unwrap();
    assert_eq!(seq, 3);
    assert_eq!(consumed, dpx_data.len());
    assert_eq!(decoded.op, WalOp::DeletePrefix);
    assert_eq!(decoded.key, b"ns:");
    assert!(decoded.value.is_empty());
    assert_eq!(decoded.mem_type, MemoryType::Procedural);
    assert_eq!(decoded.ttl_ms, 0);
    assert_eq!(decoded.timestamp_ms, 333333);
}

// ---------------------------------------------------------------------------
// TTL wire format round-trip tests (issue #18)
// Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
// ---------------------------------------------------------------------------

#[test]
fn test_ttl_zero_means_no_expiry_roundtrip() {
    // ttl_ms=0 must round-trip as no expiry (None)
    let entry = WalEntry {
        op: WalOp::Set,
        key: b"noexpiry".to_vec(),
        value: b"val".to_vec(),
        mem_type: MemoryType::Semantic,
        ttl_ms: 0,
        timestamp_ms: 1000,
        embedding: None,
    };
    let data = serialize_entry(&entry, 1);
    let (_, decoded, _) = deserialize_entry(&data).unwrap();
    assert_eq!(decoded.ttl_ms, 0);
    // Replica interprets ttl_ms==0 as None (no expiry)
    let ttl: Option<u64> = if decoded.ttl_ms == 0 {
        None
    } else {
        Some(decoded.ttl_ms)
    };
    assert_eq!(ttl, None, "ttl_ms=0 must round-trip as None (no expiry)");
}

#[test]
fn test_ttl_positive_means_actual_ttl_roundtrip() {
    // ttl_ms>0 must round-trip as an actual TTL
    let entry = WalEntry {
        op: WalOp::Set,
        key: b"withttl".to_vec(),
        value: b"val".to_vec(),
        mem_type: MemoryType::Episodic,
        ttl_ms: 5000,
        timestamp_ms: 2000,
        embedding: None,
    };
    let data = serialize_entry(&entry, 2);
    let (_, decoded, _) = deserialize_entry(&data).unwrap();
    assert_eq!(decoded.ttl_ms, 5000);
    let ttl: Option<u64> = if decoded.ttl_ms == 0 {
        None
    } else {
        Some(decoded.ttl_ms)
    };
    assert_eq!(ttl, Some(5000), "ttl_ms=5000 must round-trip as Some(5000)");
}

#[test]
fn test_ttl_large_value_roundtrip() {
    // Large TTL values must round-trip without sign extension or truncation
    let entry = WalEntry {
        op: WalOp::Set,
        key: b"largettl".to_vec(),
        value: b"val".to_vec(),
        mem_type: MemoryType::Procedural,
        ttl_ms: u64::MAX / 2,
        timestamp_ms: 3000,
        embedding: None,
    };
    let data = serialize_entry(&entry, 3);
    let (_, decoded, _) = deserialize_entry(&data).unwrap();
    assert_eq!(decoded.ttl_ms, u64::MAX / 2);
    let ttl: Option<u64> = if decoded.ttl_ms == 0 {
        None
    } else {
        Some(decoded.ttl_ms)
    };
    assert_eq!(ttl, Some(u64::MAX / 2));
}

#[test]
fn test_record_set_none_ttl_stores_zero() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let repl_state = ReplicationState::new_primary(engine, 100);
    // record_set with None TTL must store ttl_ms=0 in the WAL entry
    repl_state.record_set(b"key1", b"val1", MemoryType::Semantic, None);
    let entries = repl_state.wal().entries_from(1);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].1.ttl_ms, 0,
        "record_set with None TTL must store ttl_ms=0"
    );
}

#[test]
fn test_record_set_some_ttl_stores_value() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let repl_state = ReplicationState::new_primary(engine, 100);
    // record_set with Some(TTL) must store that TTL in the WAL entry
    repl_state.record_set(b"key2", b"val2", MemoryType::Episodic, Some(10000));
    let entries = repl_state.wal().entries_from(1);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].1.ttl_ms, 10000,
        "record_set with Some(10000) must store ttl_ms=10000"
    );
}

// ---------------------------------------------------------------------------
// Failover / lease / epoch unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_node_id_is_generated() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);
    // node_id should be a 16-char hex string
    let node_id = state.node_id();
    assert_eq!(node_id.len(), 16, "node_id should be 16 hex chars");
    assert!(
        node_id.chars().all(|c| c.is_ascii_hexdigit()),
        "node_id should be hex"
    );
}

#[test]
fn test_node_id_is_unique() {
    let engine1 = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let engine2 = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state1 = ReplicationState::new_primary(engine1, 100);
    let state2 = ReplicationState::new_primary(engine2, 100);
    assert_ne!(
        state1.node_id(),
        state2.node_id(),
        "each node should get a unique ID"
    );
}

#[test]
fn test_epoch_starts_at_zero() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);
    assert_eq!(state.current_epoch(), 0);
}

#[test]
fn test_epoch_increment() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);
    let new_epoch = state.increment_epoch();
    assert_eq!(new_epoch, 1);
    assert_eq!(state.current_epoch(), 1);
    let new_epoch2 = state.increment_epoch();
    assert_eq!(new_epoch2, 2);
    assert_eq!(state.current_epoch(), 2);
}

#[test]
fn test_epoch_set() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);
    state.set_epoch(5);
    assert_eq!(state.current_epoch(), 5);
}

#[test]
fn test_lease_tracking() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // Initially, no lease received
    assert_eq!(state.last_lease_ms(), 0);
    assert_eq!(state.primary_epoch(), 0);

    // Set lease state
    state.set_last_lease_ms(1000);
    state.set_primary_epoch(3);

    assert_eq!(state.last_lease_ms(), 1000);
    assert_eq!(state.primary_epoch(), 3);
}

#[test]
fn test_lease_expiry_detection() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // No lease received yet - should NOT be expired
    assert!(!state.is_lease_expired(20000, 15000));

    // Lease received at t=10000, now=20000, timeout=15000 -> expired (10000ms > 15000ms... wait, 10000ms elapsed < 15000ms)
    state.set_last_lease_ms(10000);
    assert!(!state.is_lease_expired(20000, 15000)); // 10000ms elapsed < 15000ms

    // Lease received at t=10000, now=26000, timeout=15000 -> expired (16000ms > 15000ms)
    assert!(state.is_lease_expired(26000, 15000)); // 16000ms elapsed > 15000ms

    // Lease received at t=10000, now=25000, timeout=15000 -> NOT expired (15000ms elapsed == timeout, should NOT be expired)
    assert!(!state.is_lease_expired(25000, 15000)); // exactly at timeout, not expired
}

#[test]
fn test_promote_to_primary() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // Set up as replica with primary epoch 2
    state.set_primary_epoch(2);
    state.set_role(ReplicationRole::Replica {
        primary_host: "localhost".to_string(),
        primary_port: 6380,
    });
    state.set_last_lease_ms(10000);

    // Promote to primary
    let new_epoch = state.promote_to_primary();
    assert_eq!(new_epoch, 3); // primary_epoch + 1
    assert_eq!(state.current_epoch(), 3);
    assert_eq!(state.role(), ReplicationRole::Primary);
    assert_eq!(state.last_lease_ms(), 0); // lease cleared
}

#[test]
fn test_demote_to_replica() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // Start as primary with epoch 0
    assert_eq!(state.role(), ReplicationRole::Primary);
    assert_eq!(state.current_epoch(), 0);

    // Demote to replica following a new primary with epoch 3
    state.demote_to_replica("10.0.0.2".to_string(), 6380, 3);
    assert_eq!(state.current_epoch(), 3);
    assert_eq!(
        state.role(),
        ReplicationRole::Replica {
            primary_host: "10.0.0.2".to_string(),
            primary_port: 6380,
        }
    );
}

#[test]
fn test_failover_config() {
    use basalt::replication::FailoverConfig;

    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // Default config should be disabled
    let config = state.failover_config();
    assert!(!config.enabled);
    assert_eq!(config.timeout_ms, 15_000);
    assert!(config.peers.is_empty());

    // Set new config
    state.set_failover_config(FailoverConfig::new(
        true,
        5000,
        vec!["10.0.0.2:6380".to_string()],
    ));
    let config = state.failover_config();
    assert!(config.enabled);
    assert_eq!(config.timeout_ms, 5000);
    assert_eq!(config.peers, vec!["10.0.0.2:6380"]);
}

#[test]
fn test_info_string_includes_failover_fields() {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    let state = ReplicationState::new_primary(engine, 100);

    // Primary info should include node_id and epoch
    let info = state.info_string();
    assert!(info.contains("node_id:"));
    assert!(info.contains("epoch:0"));
    assert!(info.contains("failover:disabled"));

    // Set failover enabled
    use basalt::replication::FailoverConfig;
    state.set_failover_config(FailoverConfig::new(true, 5000, vec![]));
    let info = state.info_string();
    assert!(info.contains("failover:enabled"));
    assert!(info.contains("failover_timeout_ms:5000"));
}
