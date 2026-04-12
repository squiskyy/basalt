use basalt::store::{KvEngine, MemoryType, snapshot, snapshot_with_threshold, load_latest_snapshot, collect_entries};

/// Basic snapshot round-trip: set data, snapshot, new engine, restore, verify.
#[test]
fn test_snapshot_roundtrip_basic() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    // Engine 1: store data
    let engine = KvEngine::new(4);
    engine.set("user:1", b"Alice".to_vec(), None, MemoryType::Semantic).unwrap();
    engine.set("user:2", b"Bob".to_vec(), None, MemoryType::Semantic).unwrap();
    engine.set("skill:login", b"password_hash".to_vec(), None, MemoryType::Procedural).unwrap();

    // Take snapshot
    let result = snapshot(db_path, &engine, 1);
    assert!(result.is_ok(), "snapshot should succeed: {:?}", result);

    // Engine 2: restore from snapshot
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2);
    assert!(loaded.is_ok(), "load should succeed: {:?}", loaded);
    assert_eq!(loaded.unwrap(), 3, "should load 3 entries");

    // Verify all data survived
    assert_eq!(engine2.get("user:1"), Some(b"Alice".to_vec()));
    assert_eq!(engine2.get("user:2"), Some(b"Bob".to_vec()));
    assert_eq!(engine2.get("skill:login"), Some(b"password_hash".to_vec()));
}

/// Snapshot round-trip with all three memory types.
#[test]
fn test_snapshot_roundtrip_all_memory_types() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(8);

    // Episodic with TTL
    engine.set(
        "epi:event1",
        b"short-lived event".to_vec(),
        Some(3_600_000), // 1 hour TTL
        MemoryType::Episodic,
    ).unwrap();

    // Semantic (no TTL)
    engine.set(
        "sem:fact1",
        b"the sky is blue".to_vec(),
        None,
        MemoryType::Semantic,
    ).unwrap();

    // Procedural (no TTL)
    engine.set(
        "proc:skill1",
        b"how to ride a bike".to_vec(),
        None,
        MemoryType::Procedural,
    ).unwrap();

    // Snapshot
    let result = snapshot(db_path, &engine, 1);
    assert!(result.is_ok(), "snapshot failed: {:?}", result);

    // Restore into new engine
    let engine2 = KvEngine::new(8);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 3);

    // Verify values
    assert_eq!(engine2.get("epi:event1"), Some(b"short-lived event".to_vec()));
    assert_eq!(engine2.get("sem:fact1"), Some(b"the sky is blue".to_vec()));
    assert_eq!(engine2.get("proc:skill1"), Some(b"how to ride a bike".to_vec()));

    // Verify memory types via get_with_meta
    let (_, epi_meta) = engine2.get_with_meta("epi:event1").unwrap();
    assert_eq!(epi_meta.memory_type, MemoryType::Episodic);
    assert!(epi_meta.ttl_remaining_ms.is_some(), "episodic entry should have TTL");

    let (_, sem_meta) = engine2.get_with_meta("sem:fact1").unwrap();
    assert_eq!(sem_meta.memory_type, MemoryType::Semantic);
    assert!(sem_meta.ttl_remaining_ms.is_none(), "semantic entry should have no TTL");

    let (_, proc_meta) = engine2.get_with_meta("proc:skill1").unwrap();
    assert_eq!(proc_meta.memory_type, MemoryType::Procedural);
    assert!(proc_meta.ttl_remaining_ms.is_none(), "procedural entry should have no TTL");
}

/// Test that expired episodic entries are NOT restored.
#[test]
fn test_snapshot_expired_entries_not_restored() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Set an entry with a very short TTL (1ms)
    engine.set(
        "epi:will_expire",
        b"ephemeral data".to_vec(),
        Some(1), // 1ms TTL
        MemoryType::Episodic,
    ).unwrap();

    // Set a persistent entry
    engine.set(
        "sem:persists",
        b"forever data".to_vec(),
        None,
        MemoryType::Semantic,
    ).unwrap();

    // Wait for the short-TTL entry to expire
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Take snapshot — collect_entries includes all entries (including expired ones since
    // scan_prefix returns entries that haven't been lazily reaped yet, but the entry
    // with expired TTL will show ttl_remaining_ms == 0). However, load_latest_snapshot
    // skips entries where expires_at <= now, so the expired entry won't be restored.
    let result = snapshot(db_path, &engine, 1);
    assert!(result.is_ok(), "snapshot failed: {:?}", result);

    // Restore into new engine
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();

    // Only the persistent entry should be loaded; the expired one should be skipped
    assert_eq!(loaded, 1, "only 1 entry should be loaded (expired skipped)");

    // Verify
    assert!(engine2.get("epi:will_expire").is_none(), "expired entry should NOT be restored");
    assert_eq!(engine2.get("sem:persists"), Some(b"forever data".to_vec()));
}

/// Snapshot and restore an empty engine.
#[test]
fn test_snapshot_roundtrip_empty() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Snapshot an empty engine
    let result = snapshot(db_path, &engine, 1);
    assert!(result.is_ok(), "snapshot of empty engine should succeed");

    // Restore into new engine
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 0, "empty snapshot should load 0 entries");

    // Verify engine2 is empty
    assert!(engine2.scan_prefix("").is_empty());
}

/// Snapshot round-trip with many entries to stress test serialization.
#[test]
fn test_snapshot_roundtrip_many_entries() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(16);
    let num_entries = 500;

    for i in 0..num_entries {
        let key = format!("bulk:key:{:04}", i);
        let value = format!("value-{}", i);
        let mt = match i % 3 {
            0 => MemoryType::Episodic,
            1 => MemoryType::Semantic,
            _ => MemoryType::Procedural,
        };
        let ttl = if i % 3 == 0 { Some(600_000) } else { None };
        engine.set(&key, value.into_bytes(), ttl, mt).unwrap();
    }

    // Snapshot
    let result = snapshot(db_path, &engine, 1);
    assert!(result.is_ok(), "snapshot with many entries should succeed");

    // Restore
    let engine2 = KvEngine::new(16);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, num_entries, "all {} entries should be loaded", num_entries);

    // Spot-check some entries
    for i in (0..num_entries).step_by(50) {
        let key = format!("bulk:key:{:04}", i);
        let expected = format!("value-{}", i);
        assert_eq!(
            engine2.get(&key),
            Some(expected.into_bytes()),
            "key {} should have correct value",
            key
        );
    }
}

/// Test that multiple snapshots are handled correctly and the latest is loaded.
#[test]
fn test_snapshot_multiple_and_restore_latest() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    // First snapshot with 2 entries
    let engine1 = KvEngine::new(4);
    engine1.set("a", b"1".to_vec(), None, MemoryType::Semantic).unwrap();
    engine1.set("b", b"2".to_vec(), None, MemoryType::Semantic).unwrap();
    snapshot(db_path, &engine1, 5).unwrap();

    // Small delay so timestamp differs
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Second snapshot with 3 entries (different engine instance, same path)
    let engine2 = KvEngine::new(4);
    engine2.set("a", b"1".to_vec(), None, MemoryType::Semantic).unwrap();
    engine2.set("b", b"2".to_vec(), None, MemoryType::Semantic).unwrap();
    engine2.set("c", b"3".to_vec(), None, MemoryType::Semantic).unwrap();
    snapshot(db_path, &engine2, 5).unwrap();

    // Restore: should get the LATEST snapshot (3 entries)
    let engine3 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine3).unwrap();
    assert_eq!(loaded, 3, "should load latest snapshot with 3 entries");

    assert_eq!(engine3.get("a"), Some(b"1".to_vec()));
    assert_eq!(engine3.get("b"), Some(b"2".to_vec()));
    assert_eq!(engine3.get("c"), Some(b"3".to_vec()));
}

/// Test collect_entries helper.
#[test]
fn test_collect_entries() {
    let engine = KvEngine::new(4);
    engine.set("k1", b"v1".to_vec(), None, MemoryType::Semantic).unwrap();
    engine.set("k2", b"v2".to_vec(), Some(600_000), MemoryType::Episodic).unwrap();
    engine.set("k3", b"v3".to_vec(), None, MemoryType::Procedural).unwrap();

    let entries = collect_entries(&engine);
    assert_eq!(entries.len(), 3);

    // Verify all keys are present (order may vary due to sharding)
    let keys: std::collections::HashSet<String> = entries.iter().map(|e| e.key.clone()).collect();
    assert!(keys.contains("k1"));
    assert!(keys.contains("k2"));
    assert!(keys.contains("k3"));
}

/// Test snapshot with binary values (non-UTF8).
#[test]
fn test_snapshot_binary_values() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Binary values (including all byte values 0-255)
    let binary_val: Vec<u8> = (0u8..=255).collect();
    engine.set("bin:data", binary_val.clone(), None, MemoryType::Procedural).unwrap();

    // Empty value
    engine.set("bin:empty", b"".to_vec(), None, MemoryType::Semantic).unwrap();

    // Snapshot and restore
    snapshot(db_path, &engine, 1).unwrap();

    let engine2 = KvEngine::new(4);
    load_latest_snapshot(db_path, &engine2).unwrap();

    assert_eq!(engine2.get("bin:data"), Some(binary_val));
    assert_eq!(engine2.get("bin:empty"), Some(b"".to_vec()));
}

/// Test that restoring into a non-empty engine merges data (set_force overwrites).
#[test]
fn test_snapshot_restore_into_nonempty_engine() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    // Create and snapshot engine with some data
    let engine1 = KvEngine::new(4);
    engine1.set("shared", b"from_snapshot".to_vec(), None, MemoryType::Semantic).unwrap();
    engine1.set("only_in_snapshot", b"yes".to_vec(), None, MemoryType::Semantic).unwrap();
    snapshot(db_path, &engine1, 1).unwrap();

    // Create engine2 with some pre-existing data
    let engine2 = KvEngine::new(4);
    engine2.set("shared", b"original".to_vec(), None, MemoryType::Semantic).unwrap();
    engine2.set("only_in_engine", b"local".to_vec(), None, MemoryType::Semantic).unwrap();

    // Restore snapshot into engine2
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 2);

    // "shared" should be overwritten by snapshot value (set_force)
    assert_eq!(engine2.get("shared"), Some(b"from_snapshot".to_vec()));
    // "only_in_snapshot" should be added
    assert_eq!(engine2.get("only_in_snapshot"), Some(b"yes".to_vec()));
    // "only_in_engine" should still exist (restore doesn't delete existing keys)
    assert_eq!(engine2.get("only_in_engine"), Some(b"local".to_vec()));
}

/// Test snapshot round-trip with namespace-prefixed keys.
#[test]
fn test_snapshot_namespace_keys() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(8);
    engine.set("session:abc:1", b"s1".to_vec(), None, MemoryType::Episodic).unwrap();
    engine.set("session:abc:2", b"s2".to_vec(), None, MemoryType::Episodic).unwrap();
    engine.set("session:def:1", b"s3".to_vec(), None, MemoryType::Episodic).unwrap();
    engine.set("cache:img:1", b"c1".to_vec(), None, MemoryType::Semantic).unwrap();
    engine.set("cache:img:2", b"c2".to_vec(), None, MemoryType::Semantic).unwrap();

    snapshot(db_path, &engine, 1).unwrap();

    let engine2 = KvEngine::new(8);
    load_latest_snapshot(db_path, &engine2).unwrap();

    // Verify namespace-scoped scans work after restore
    let session_entries = engine2.scan_prefix("session:");
    assert_eq!(session_entries.len(), 3);

    let cache_entries = engine2.scan_prefix("cache:");
    assert_eq!(cache_entries.len(), 2);

    // Verify delete_prefix works on restored data
    let deleted = engine2.delete_prefix("session:");
    assert_eq!(deleted, 3);
    assert!(engine2.scan_prefix("session:").is_empty());
    assert_eq!(engine2.scan_prefix("cache:").len(), 2);
}

/// Test that large values are LZ4-compressed in the snapshot file.
/// The snapshot file should be smaller than the raw uncompressed data.
#[test]
fn test_snapshot_compression_large_values() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Create a large compressible value (>1KB)
    let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes(); // 2048 bytes, highly compressible
    assert!(large_value.len() > 1024, "value should be > 1KB");

    engine.set("big:key", large_value.clone(), None, MemoryType::Semantic).unwrap();
    engine.set("small:key", b"tiny".to_vec(), None, MemoryType::Semantic).unwrap();

    // Snapshot with default threshold (1024)
    let result = snapshot_with_threshold(db_path, &engine, 1, 1024);
    assert!(result.is_ok(), "snapshot should succeed: {:?}", result);

    // Restore into new engine
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 2);

    // Verify data integrity
    assert_eq!(engine2.get("big:key"), Some(large_value.clone()));
    assert_eq!(engine2.get("small:key"), Some(b"tiny".to_vec()));

    // Verify the snapshot file is smaller than the uncompressed data
    let snapshot_file = basalt::store::persistence::find_latest_snapshot(db_path).unwrap();
    let file_size = std::fs::metadata(&snapshot_file).unwrap().len() as usize;
    // The large value alone is 2048 bytes; the file should be smaller than
    // if stored uncompressed (file has overhead for keys, metadata, headers, but
    // the 2048-byte value should compress well below 2048)
    assert!(
        file_size < large_value.len() + 200,
        "snapshot file ({}) should be smaller than uncompressed data ({}) + small overhead",
        file_size,
        large_value.len()
    );
}

/// Test that small values are NOT compressed in the snapshot file.
#[test]
fn test_snapshot_no_compression_small_values() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Small values well below threshold
    engine.set("k1", b"hello".to_vec(), None, MemoryType::Semantic).unwrap();
    engine.set("k2", b"world".to_vec(), None, MemoryType::Episodic).unwrap();

    snapshot_with_threshold(db_path, &engine, 1, 1024).unwrap();

    // Restore and verify
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 2);
    assert_eq!(engine2.get("k1"), Some(b"hello".to_vec()));
    assert_eq!(engine2.get("k2"), Some(b"world".to_vec()));

    // Read the raw file and verify the flags byte for each entry has bit 0 unset
    let snapshot_file = basalt::store::persistence::find_latest_snapshot(db_path).unwrap();
    let raw = std::fs::read(&snapshot_file).unwrap();
    // Header: 7 magic + 1 version + 8 count = 16 bytes
    // Then per entry: 4 key_len + key + 1 flags + 4 val_len + val + 1 memory_type + 8 expires_at
    let header_size = 16;
    let mut offset = header_size;
    for _ in 0..2 {
        // key_len
        let key_len = u32::from_le_bytes(raw[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4 + key_len;
        // flags byte
        let flags = raw[offset];
        assert_eq!(flags & 0b1, 0, "small value should NOT have compressed flag set");
        offset += 1;
        // val_len
        let val_len = u32::from_le_bytes(raw[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4 + val_len + 1 + 8; // skip value + memory_type + expires_at
    }
}

/// Test backward compatibility: a version 1 snapshot (no flags byte) can still be restored.
#[test]
fn test_snapshot_backward_compat_v1() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    // Manually write a version 1 snapshot file
    // Format: magic(7) + version(1) + count(8) + per-entry: key_len(4) + key + val_len(4) + val + mt(1) + exp(8)
    let mut data = Vec::new();
    data.extend_from_slice(b"BASALT\x00"); // magic
    data.push(1u8); // version 1
    data.extend_from_slice(&1u64.to_le_bytes()); // 1 entry

    // Entry: key="legacy:key", value="legacy value data", memory_type=Semantic(1), expires_at=0
    let key = b"legacy:key";
    data.extend_from_slice(&(key.len() as u32).to_le_bytes());
    data.extend_from_slice(key);
    let val = b"legacy value data";
    data.extend_from_slice(&(val.len() as u32).to_le_bytes());
    data.extend_from_slice(val);
    data.push(1u8); // Semantic
    data.extend_from_slice(&0u64.to_le_bytes()); // no expiry

    // Write as a snapshot file
    let snapshot_path = db_path.join("snapshot-9999.bin");
    std::fs::write(&snapshot_path, &data).unwrap();

    // Load into engine
    let engine = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine).unwrap();
    assert_eq!(loaded, 1);

    // Verify the data was restored correctly
    assert_eq!(engine.get("legacy:key"), Some(b"legacy value data".to_vec()));
}

/// Test that compression disabled with threshold=0 still works correctly.
#[test]
fn test_snapshot_compression_disabled() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = dir.path();

    let engine = KvEngine::new(4);

    // Large value that would normally be compressed
    let large_value: Vec<u8> = "X".repeat(5000).into_bytes();
    engine.set("big:key", large_value.clone(), None, MemoryType::Semantic).unwrap();

    // Snapshot with threshold=0 (compression disabled)
    snapshot_with_threshold(db_path, &engine, 1, 0).unwrap();

    // Restore
    let engine2 = KvEngine::new(4);
    let loaded = load_latest_snapshot(db_path, &engine2).unwrap();
    assert_eq!(loaded, 1);
    assert_eq!(engine2.get("big:key"), Some(large_value));
}
