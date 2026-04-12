//! Replication end-to-end tests (Issue #31).
//!
//! These tests verify that primary-replica sync works by:
//! - Starting two KvEngine instances
//! - Setting up replication using the existing WAL and replication protocol
//! - Writing data on the primary
//! - Verifying that the replica applies WAL entries and ends up with the same data
//!
//! Since the full network-based replication requires TCP connections and async
//! coordination, we test the core replication pipeline at the engine + WAL level:
//! 1. Primary records writes in its WAL
//! 2. WAL entries are serialized and deserialized (simulating network transfer)
//! 3. Replica applies the deserialized entries to its local engine
//! 4. Verify the replica engine has the same data as the primary

use basalt::replication::wal::{Wal, WalEntry, WalOp, serialize_entry, deserialize_entry};
use basalt::replication::{ReplicationState, ReplicationRole};
use basalt::store::engine::KvEngine;
use basalt::store::memory_type::MemoryType;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// E2E replication tests: WAL-based primary -> replica sync
// ---------------------------------------------------------------------------

/// Test basic primary-replica sync: write SET on primary, serialize WAL,
/// deserialize on replica, apply to replica engine, verify data matches.
#[test]
fn test_e2e_replication_set_sync() {
    let primary_engine = Arc::new(KvEngine::new(4));
    let replica_engine = Arc::new(KvEngine::new(4));

    let primary_state = ReplicationState::new_primary(primary_engine.clone(), 1000);

    // Write data on the primary
    primary_engine
        .set("ns:key1", b"value1".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    primary_state.record_set(b"ns:key1", b"value1", MemoryType::Semantic, None);

    primary_engine
        .set("ns:key2", b"value2".to_vec(), None, MemoryType::Episodic)
        .unwrap();
    primary_state.record_set(b"ns:key2", b"value2", MemoryType::Episodic, Some(3600000));

    // Simulate WAL streaming: serialize and deserialize each entry
    let entries = primary_state.wal().entries_from(1);
    for (seq, entry) in &entries {
        let binary = serialize_entry(entry, *seq);
        let (decoded_seq, decoded_entry, _consumed) =
            deserialize_entry(&binary).expect("deserialization should succeed");

        // Apply to replica engine
        match decoded_entry.op {
            WalOp::Set => {
                let ttl = if decoded_entry.ttl_ms == 0 {
                    None
                } else {
                    Some(decoded_entry.ttl_ms)
                };
                replica_engine.set_force_with_embedding(
                    &String::from_utf8_lossy(&decoded_entry.key),
                    decoded_entry.value.clone(),
                    ttl,
                    decoded_entry.mem_type,
                    decoded_entry.embedding.clone(),
                );
            }
            WalOp::Delete => {
                replica_engine.delete(&String::from_utf8_lossy(&decoded_entry.key));
            }
            WalOp::DeletePrefix => {
                replica_engine.delete_prefix(&String::from_utf8_lossy(&decoded_entry.key));
            }
        }

        assert_eq!(decoded_seq, *seq);
    }

    // Verify replica has the same data
    assert_eq!(
        replica_engine.get("ns:key1"),
        Some(b"value1".to_vec()),
        "replica should have key1"
    );
    assert_eq!(
        replica_engine.get("ns:key2"),
        Some(b"value2".to_vec()),
        "replica should have key2"
    );

    // Verify memory types
    let (_, meta1) = replica_engine.get_with_meta("ns:key1").unwrap();
    assert_eq!(meta1.memory_type, MemoryType::Semantic);
    assert!(meta1.ttl_remaining_ms.is_none());

    let (_, meta2) = replica_engine.get_with_meta("ns:key2").unwrap();
    assert_eq!(meta2.memory_type, MemoryType::Episodic);
    assert!(meta2.ttl_remaining_ms.is_some());
}

/// Test replication of DELETE operations: write then delete on primary,
/// replicate WAL, verify key is absent on replica.
#[test]
fn test_e2e_replication_delete_sync() {
    let primary_engine = Arc::new(KvEngine::new(4));
    let replica_engine = Arc::new(KvEngine::new(4));

    let primary_state = ReplicationState::new_primary(primary_engine.clone(), 1000);

    // Write a key on primary
    primary_engine
        .set("ns:delkey", b"will_be_deleted".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    primary_state.record_set(b"ns:delkey", b"will_be_deleted", MemoryType::Semantic, None);

    // Delete the key on primary
    primary_engine.delete("ns:delkey");
    primary_state.record_delete(b"ns:delkey");

    // Replicate all WAL entries to replica
    let entries = primary_state.wal().entries_from(1);
    for (seq, entry) in &entries {
        let binary = serialize_entry(entry, *seq);
        let (_, decoded_entry, _) = deserialize_entry(&binary).unwrap();

        match decoded_entry.op {
            WalOp::Set => {
                let ttl = if decoded_entry.ttl_ms == 0 { None } else { Some(decoded_entry.ttl_ms) };
                replica_engine.set_force_with_embedding(
                    &String::from_utf8_lossy(&decoded_entry.key),
                    decoded_entry.value.clone(),
                    ttl,
                    decoded_entry.mem_type,
                    decoded_entry.embedding.clone(),
                );
            }
            WalOp::Delete => {
                replica_engine.delete(&String::from_utf8_lossy(&decoded_entry.key));
            }
            WalOp::DeletePrefix => {
                replica_engine.delete_prefix(&String::from_utf8_lossy(&decoded_entry.key));
            }
        }
    }

    // Key should be absent on replica
    assert!(
        replica_engine.get("ns:delkey").is_none(),
        "deleted key should not exist on replica"
    );
}

/// Test replication of DELETE_PREFIX operations.
#[test]
fn test_e2e_replication_delete_prefix_sync() {
    let primary_engine = Arc::new(KvEngine::new(4));
    let replica_engine = Arc::new(KvEngine::new(4));

    let primary_state = ReplicationState::new_primary(primary_engine.clone(), 1000);

    // Write several keys in a namespace
    for i in 0..5u8 {
        let key = format!("ns:key{i}");
        let val = format!("val{i}");
        primary_engine
            .set(&key, val.as_bytes().to_vec(), None, MemoryType::Semantic)
            .unwrap();
        primary_state.record_set(key.as_bytes(), val.as_bytes(), MemoryType::Semantic, None);
    }

    // Also write a key in a different namespace
    primary_engine
        .set("other:safe", b"safe".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    primary_state.record_set(b"other:safe", b"safe", MemoryType::Semantic, None);

    // Delete the entire "ns:" prefix on primary
    primary_engine.delete_prefix("ns:");
    primary_state.record_delete_prefix(b"ns:");

    // Replicate all WAL entries to replica
    let entries = primary_state.wal().entries_from(1);
    for (seq, entry) in &entries {
        let binary = serialize_entry(entry, *seq);
        let (_, decoded_entry, _) = deserialize_entry(&binary).unwrap();

        match decoded_entry.op {
            WalOp::Set => {
                let ttl = if decoded_entry.ttl_ms == 0 { None } else { Some(decoded_entry.ttl_ms) };
                replica_engine.set_force_with_embedding(
                    &String::from_utf8_lossy(&decoded_entry.key),
                    decoded_entry.value.clone(),
                    ttl,
                    decoded_entry.mem_type,
                    decoded_entry.embedding.clone(),
                );
            }
            WalOp::Delete => {
                replica_engine.delete(&String::from_utf8_lossy(&decoded_entry.key));
            }
            WalOp::DeletePrefix => {
                replica_engine.delete_prefix(&String::from_utf8_lossy(&decoded_entry.key));
            }
        }
    }

    // "ns:" keys should be gone on replica
    for i in 0..5u8 {
        let key = format!("ns:key{i}");
        assert!(
            replica_engine.get(&key).is_none(),
            "key {key} should be deleted on replica"
        );
    }

    // "other:safe" should still exist
    assert_eq!(
        replica_engine.get("other:safe"),
        Some(b"safe".to_vec()),
        "other namespace key should survive prefix delete"
    );
}

/// Test full snapshot + WAL replication: first apply snapshot from primary,
/// then stream WAL entries, and verify replica matches primary.
#[test]
fn test_e2e_replication_snapshot_plus_wal() {
    let primary_engine = Arc::new(KvEngine::new(4));
    let replica_engine = Arc::new(KvEngine::new(4));

    let primary_state = ReplicationState::new_primary(primary_engine.clone(), 1000);

    // Phase 1: Write initial data on primary (this simulates the snapshot phase)
    primary_engine
        .set("data:a", b"alpha".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    primary_state.record_set(b"data:a", b"alpha", MemoryType::Semantic, None);

    primary_engine
        .set("data:b", b"beta".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    primary_state.record_set(b"data:b", b"beta", MemoryType::Semantic, None);

    // Simulate snapshot: replica gets all current data from primary
    let snapshot_entries = primary_engine.scan_prefix_with_embeddings("");
    for (key, value, meta) in &snapshot_entries {
        let ttl = meta.ttl_remaining_ms;
        replica_engine.set_force_with_embedding(
            key,
            value.clone(),
            ttl,
            meta.memory_type,
            meta.embedding.clone(),
        );
    }

    // Verify snapshot data on replica
    assert_eq!(replica_engine.get("data:a"), Some(b"alpha".to_vec()));
    assert_eq!(replica_engine.get("data:b"), Some(b"beta".to_vec()));

    // Record the WAL offset after snapshot
    let snapshot_seq = primary_state.replication_offset();

    // Phase 2: Write new data on primary after the snapshot (WAL streaming phase)
    primary_engine
        .set("data:c", b"gamma".to_vec(), None, MemoryType::Episodic)
        .unwrap();
    primary_state.record_set(b"data:c", b"gamma", MemoryType::Episodic, Some(60000));

    primary_engine
        .set("data:d", b"delta".to_vec(), None, MemoryType::Procedural)
        .unwrap();
    primary_state.record_set(b"data:d", b"delta", MemoryType::Procedural, None);

    // Stream only new WAL entries (after snapshot point)
    let new_entries = primary_state.wal().entries_from(snapshot_seq + 1);
    for (seq, entry) in &new_entries {
        let binary = serialize_entry(entry, *seq);
        let (_, decoded_entry, _) = deserialize_entry(&binary).unwrap();

        match decoded_entry.op {
            WalOp::Set => {
                let ttl = if decoded_entry.ttl_ms == 0 { None } else { Some(decoded_entry.ttl_ms) };
                replica_engine.set_force_with_embedding(
                    &String::from_utf8_lossy(&decoded_entry.key),
                    decoded_entry.value.clone(),
                    ttl,
                    decoded_entry.mem_type,
                    decoded_entry.embedding.clone(),
                );
            }
            WalOp::Delete => {
                replica_engine.delete(&String::from_utf8_lossy(&decoded_entry.key));
            }
            WalOp::DeletePrefix => {
                replica_engine.delete_prefix(&String::from_utf8_lossy(&decoded_entry.key));
            }
        }
    }

    // Verify all data on replica
    assert_eq!(replica_engine.get("data:a"), Some(b"alpha".to_vec()));
    assert_eq!(replica_engine.get("data:b"), Some(b"beta".to_vec()));
    assert_eq!(replica_engine.get("data:c"), Some(b"gamma".to_vec()));
    assert_eq!(replica_engine.get("data:d"), Some(b"delta".to_vec()));

    // Verify memory types for WAL-streamed entries
    let (_, meta_c) = replica_engine.get_with_meta("data:c").unwrap();
    assert_eq!(meta_c.memory_type, MemoryType::Episodic);

    let (_, meta_d) = replica_engine.get_with_meta("data:d").unwrap();
    assert_eq!(meta_d.memory_type, MemoryType::Procedural);
}

/// Test replication with embedding vectors: primary stores entries with
/// embeddings, replica receives them via WAL, and search works on replica.
#[test]
fn test_e2e_replication_with_embeddings() {
    let primary_engine = Arc::new(KvEngine::new(4));
    let replica_engine = Arc::new(KvEngine::new(4));

    let primary_state = ReplicationState::new_primary(primary_engine.clone(), 1000);

    // Store entries with embeddings on primary
    primary_engine
        .set_with_embedding(
            "vec:cat",
            b"feline pet".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![1.0, 0.0, 0.0]),
        )
        .unwrap();
    primary_state.record_set_with_embedding(
        b"vec:cat",
        b"feline pet",
        MemoryType::Semantic,
        None,
        Some(vec![1.0, 0.0, 0.0]),
    );

    primary_engine
        .set_with_embedding(
            "vec:dog",
            b"canine pet".to_vec(),
            None,
            MemoryType::Semantic,
            Some(vec![0.0, 1.0, 0.0]),
        )
        .unwrap();
    primary_state.record_set_with_embedding(
        b"vec:dog",
        b"canine pet",
        MemoryType::Semantic,
        None,
        Some(vec![0.0, 1.0, 0.0]),
    );

    // Replicate WAL to replica
    let entries = primary_state.wal().entries_from(1);
    for (seq, entry) in &entries {
        let binary = serialize_entry(entry, *seq);
        let (_, decoded_entry, _) = deserialize_entry(&binary).unwrap();

        match decoded_entry.op {
            WalOp::Set => {
                let ttl = if decoded_entry.ttl_ms == 0 { None } else { Some(decoded_entry.ttl_ms) };
                replica_engine.set_force_with_embedding(
                    &String::from_utf8_lossy(&decoded_entry.key),
                    decoded_entry.value.clone(),
                    ttl,
                    decoded_entry.mem_type,
                    decoded_entry.embedding.clone(),
                );
            }
            WalOp::Delete => {
                replica_engine.delete(&String::from_utf8_lossy(&decoded_entry.key));
            }
            WalOp::DeletePrefix => {
                replica_engine.delete_prefix(&String::from_utf8_lossy(&decoded_entry.key));
            }
        }
    }

    // Verify replica has the data
    assert_eq!(replica_engine.get("vec:cat"), Some(b"feline pet".to_vec()));
    assert_eq!(replica_engine.get("vec:dog"), Some(b"canine pet".to_vec()));

    // Verify search works on replica with replicated embeddings
    let results = replica_engine.search_embedding("vec", &[1.0, 0.0, 0.0], 2);
    assert_eq!(results.len(), 2, "replica search should return 2 results");
    assert_eq!(results[0].key, "cat", "closest result should be 'cat'");
    // Results sorted by distance
    for i in 1..results.len() {
        assert!(
            results[i].distance >= results[i - 1].distance,
            "results should be sorted by distance"
        );
    }
}

/// Test that a ReplicationState starts as Primary with correct defaults.
#[test]
fn test_e2e_replication_state_initial() {
    let engine = Arc::new(KvEngine::new(4));
    let state = ReplicationState::new_primary(engine, 500);

    assert_eq!(state.role(), ReplicationRole::Primary);
    assert_eq!(state.connected_replicas(), 0);
    assert_eq!(state.replication_offset(), 0);
    assert!(state.wal().is_empty());
}

/// Test that recording multiple operations advances the replication offset.
#[test]
fn test_e2e_replication_offset_advances() {
    let engine = Arc::new(KvEngine::new(4));
    let state = ReplicationState::new_primary(engine, 1000);

    assert_eq!(state.replication_offset(), 0);

    state.record_set(b"k1", b"v1", MemoryType::Semantic, None);
    assert_eq!(state.replication_offset(), 1);

    state.record_set(b"k2", b"v2", MemoryType::Episodic, Some(5000));
    assert_eq!(state.replication_offset(), 2);

    state.record_delete(b"k1");
    assert_eq!(state.replication_offset(), 3);

    state.record_delete_prefix(b"ns:");
    assert_eq!(state.replication_offset(), 4);
}

/// Test WAL circular buffer with replication offset tracking.
#[test]
fn test_e2e_replication_wal_circular_with_offset() {
    let engine = Arc::new(KvEngine::new(4));
    // Small WAL (max 5 entries) to test circular behavior
    let state = ReplicationState::new_primary(engine, 5);

    // Write 8 entries - WAL will only keep the last 5
    for i in 0..8u64 {
        let key = format!("key{i}");
        let val = format!("val{i}");
        state.record_set(key.as_bytes(), val.as_bytes(), MemoryType::Semantic, None);
    }

    // Offset should be 8
    assert_eq!(state.replication_offset(), 8);

    // WAL should have 5 entries (the most recent 5)
    assert_eq!(state.wal().len(), 5);

    // Oldest seq should be 4 (entries 4, 5, 6, 7, 8)
    assert_eq!(state.wal().oldest_seq(), Some(4));

    // We can still read from seq 4 onward
    let entries = state.wal().entries_from(4);
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].0, 4);
    assert_eq!(entries[4].0, 8);
}
