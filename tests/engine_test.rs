use basalt::store::{KvEngine, MemoryType};

#[test]
fn test_set_get_basic() {
    let engine = KvEngine::new(4);
    engine
        .set("hello", b"world".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    let val = engine.get("hello").unwrap();
    assert_eq!(val, b"world");

    // Non-existent key
    assert!(engine.get("missing").is_none());
}

#[test]
fn test_set_get_with_ttl() {
    let engine = KvEngine::new(4);
    // Set with a TTL of 1 ms — should expire almost immediately
    engine
        .set(
            "short_lived",
            b"gone".to_vec(),
            Some(1),
            MemoryType::Episodic,
        )
        .unwrap();

    // Set a persistent key
    engine
        .set("persistent", b"here".to_vec(), None, MemoryType::Semantic)
        .unwrap();

    // Wait a bit for the TTL to expire
    std::thread::sleep(std::time::Duration::from_millis(10));

    // The short-lived key should be expired (lazy expiry on read)
    assert!(engine.get("short_lived").is_none());

    // The persistent key should still be there
    assert_eq!(engine.get("persistent").unwrap(), b"here");
}

#[test]
fn test_delete() {
    let engine = KvEngine::new(4);
    engine
        .set("to_delete", b"value".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    assert!(engine.delete("to_delete"));
    assert!(!engine.delete("to_delete")); // already deleted
    assert!(engine.get("to_delete").is_none());
}

#[test]
fn test_namespace_via_prefix() {
    let engine = KvEngine::new(4);
    // Use "namespace:key" internal format
    engine
        .set("session:abc", b"data1".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("session:def", b"data2".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("cache:xyz", b"data3".to_vec(), None, MemoryType::Semantic)
        .unwrap();

    // Scan the "session:" namespace
    let session_entries = engine.scan_prefix("session:");
    assert_eq!(session_entries.len(), 2);

    // Scan the "cache:" namespace
    let cache_entries = engine.scan_prefix("cache:");
    assert_eq!(cache_entries.len(), 1);
    assert_eq!(cache_entries[0].1, b"data3".to_vec());
}

#[test]
fn test_delete_prefix() {
    let engine = KvEngine::new(8);
    engine
        .set("ns:a", b"1".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("ns:b", b"2".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("ns:c", b"3".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    engine
        .set("other:d", b"4".to_vec(), None, MemoryType::Semantic)
        .unwrap();

    let removed = engine.delete_prefix("ns:");
    assert_eq!(removed, 3);

    // "ns:" keys should be gone
    assert!(engine.get("ns:a").is_none());
    assert!(engine.get("ns:b").is_none());
    assert!(engine.get("ns:c").is_none());

    // "other:" key should still exist
    assert_eq!(engine.get("other:d").unwrap(), b"4".to_vec());
}

#[test]
fn test_get_with_meta() {
    let engine = KvEngine::new(4);
    engine
        .set(
            "meta_key",
            b"meta_val".to_vec(),
            Some(10_000),
            MemoryType::Episodic,
        )
        .unwrap();

    let (value, meta) = engine.get_with_meta("meta_key").unwrap();
    assert_eq!(value, b"meta_val");
    assert_eq!(meta.memory_type, MemoryType::Episodic);
    // TTL remaining should be close to 10_000 ms
    assert!(meta.ttl_remaining_ms.is_some());
    let remaining = meta.ttl_remaining_ms.unwrap();
    assert!(
        remaining > 9_000,
        "remaining ttl should be near 10s, got {remaining}"
    );
    assert!(
        remaining <= 10_000,
        "remaining ttl should not exceed 10s, got {remaining}"
    );

    // Test a key with no TTL (Semantic)
    engine
        .set("no_ttl", b"val".to_vec(), None, MemoryType::Semantic)
        .unwrap();
    let (_, meta2) = engine.get_with_meta("no_ttl").unwrap();
    assert_eq!(meta2.memory_type, MemoryType::Semantic);
    assert!(meta2.ttl_remaining_ms.is_none());
}

#[test]
fn test_episodic_has_ttl() {
    // Episodic memory type should have a default TTL
    let ttl = MemoryType::Episodic.default_ttl_ms();
    assert!(ttl.is_some(), "Episodic should have a default TTL");
    assert_eq!(
        ttl.unwrap(),
        3_600_000,
        "Default episodic TTL should be 3_600_000 ms (1 hour)"
    );

    // Semantic and Procedural should not have a default TTL
    assert!(MemoryType::Semantic.default_ttl_ms().is_none());
    assert!(MemoryType::Procedural.default_ttl_ms().is_none());

    // Verify that using the default TTL with the engine works
    let engine = KvEngine::new(4);
    let ttl = MemoryType::Episodic.default_ttl_ms();
    engine
        .set(
            "episodic_key",
            b"ephemeral".to_vec(),
            ttl,
            MemoryType::Episodic,
        )
        .unwrap();

    let (_, meta) = engine.get_with_meta("episodic_key").unwrap();
    assert_eq!(meta.memory_type, MemoryType::Episodic);
    assert!(meta.ttl_remaining_ms.is_some());
}
