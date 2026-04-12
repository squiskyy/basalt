use basalt::replication::wal::{Wal, WalEntry, WalOp, serialize_entry, deserialize_entry};
use basalt::replication::{ReplicationState, ReplicationRole};
use basalt::store::engine::KvEngine;
use basalt::store::memory_type::MemoryType;
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
    };
    let e2 = WalEntry {
        op: WalOp::Set,
        key: b"key2".to_vec(),
        value: b"val2".to_vec(),
        mem_type: MemoryType::Episodic,
        ttl_ms: 5000,
        timestamp_ms: 200,
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
    });
    assert_eq!(seq3, 3);
    assert_eq!(wal.current_seq(), 4);
}

// ---------------------------------------------------------------------------
// ReplicationState unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_replication_state_primary() {
    let engine = Arc::new(KvEngine::new(4));
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
    let engine = Arc::new(KvEngine::new(4));
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
    let engine = Arc::new(KvEngine::new(4));
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
