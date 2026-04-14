use basalt::store::{ConsolidationManager, KvEngine, MemoryType};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Spawn 100+ tokio tasks all doing set/get/delete concurrently on the same KvEngine.
/// Verify no panics and correct final state.
#[tokio::test]
async fn test_concurrent_set_get_delete() {
    let engine = Arc::new(KvEngine::new(16, Arc::new(ConsolidationManager::disabled())));
    let num_tasks = 128;
    let ops_per_task = 50;

    let mut handles = Vec::new();

    for i in 0..num_tasks {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for j in 0..ops_per_task {
                let key = format!("task{}:key{}", i, j);
                let value = format!("value-{}-{}", i, j);

                // Set
                e.set(&key, value.as_bytes().to_vec(), None, MemoryType::Semantic)
                    .unwrap();

                // Get
                let got = e.get(&key);
                assert_eq!(
                    got,
                    Some(value.as_bytes().to_vec()),
                    "get after set failed for {}",
                    key
                );

                // Delete
                let deleted = e.delete(&key);
                assert!(deleted, "delete should return true for {}", key);

                // Get after delete
                let gone = e.get(&key);
                assert!(
                    gone.is_none(),
                    "get after delete should return None for {}",
                    key
                );
            }
        }));
    }

    // All tasks must complete without panic
    for handle in handles {
        handle.await.unwrap();
    }
}

/// Test concurrent reads and writes to the SAME keys from many tasks.
#[tokio::test]
async fn test_concurrent_same_key_rw() {
    let engine = Arc::new(KvEngine::new(16, Arc::new(ConsolidationManager::disabled())));
    let num_tasks = 100;
    let num_rounds = 200;

    // Pre-populate a shared key
    engine
        .set("shared", b"initial".to_vec(), None, MemoryType::Semantic)
        .unwrap();

    let write_count = Arc::new(AtomicUsize::new(0));
    let read_count = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();

    for task_id in 0..num_tasks {
        let e = Arc::clone(&engine);
        let wc = Arc::clone(&write_count);
        let rc = Arc::clone(&read_count);

        handles.push(tokio::spawn(async move {
            for _ in 0..num_rounds {
                if task_id % 2 == 0 {
                    // Writer: set new value
                    let val = format!("writer-{}", task_id);
                    e.set(
                        "shared",
                        val.as_bytes().to_vec(),
                        None,
                        MemoryType::Semantic,
                    )
                    .unwrap();
                    wc.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Reader: get value (may be any writer's value, but must be valid UTF-8)
                    let got = e.get("shared");
                    assert!(got.is_some(), "shared key should always exist");
                    rc.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // The shared key must still exist with some value
    let final_val = engine.get("shared");
    assert!(
        final_val.is_some(),
        "shared key must still exist after concurrent rw"
    );
}

/// Test concurrent namespace operations (scan_prefix, delete_prefix) interleaved with set/get.
#[tokio::test]
async fn test_concurrent_namespace_operations() {
    let engine = Arc::new(KvEngine::new(16, Arc::new(ConsolidationManager::disabled())));

    // Pre-populate two namespaces
    for i in 0..50 {
        engine
            .set(
                &format!("ns1:k{}", i),
                format!("v1-{}", i).into_bytes(),
                None,
                MemoryType::Semantic,
            )
            .unwrap();
        engine
            .set(
                &format!("ns2:k{}", i),
                format!("v2-{}", i).into_bytes(),
                None,
                MemoryType::Semantic,
            )
            .unwrap();
    }

    let mut handles = Vec::new();

    // Task group 1: scan ns1
    {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for _ in 0..100 {
                let results = e.scan_prefix("ns1:");
                // Results may vary due to concurrent deletes, but should not panic
                assert!(results.len() <= 50);
            }
        }));
    }

    // Task group 2: delete ns1 keys one by one
    {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for i in 0..50 {
                e.delete(&format!("ns1:k{}", i));
            }
        }));
    }

    // Task group 3: add new keys to ns2
    {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for i in 50..100 {
                e.set(
                    &format!("ns2:k{}", i),
                    format!("v2-{}", i).into_bytes(),
                    None,
                    MemoryType::Semantic,
                )
                .unwrap();
            }
        }));
    }

    // Task group 4: delete_prefix on ns2
    {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            // Small delay to let some ns2 keys exist first
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            let _removed = e.delete_prefix("ns2:");
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // After all operations, engine should be in a consistent state
    // ns1 keys were individually deleted
    let ns1_remaining = engine.scan_prefix("ns1:");
    // ns2 was bulk-deleted (but some may have been added after the delete_prefix)
    let ns2_remaining = engine.scan_prefix("ns2:");
    // Just verify no panics and results are reasonable
    assert!(ns1_remaining.len() <= 50);
    assert!(ns2_remaining.len() <= 100);
}

/// Test mixed workload with TTL expiry under concurrent access.
/// Set entries with very short TTL, then verify expired entries don't cause panics.
#[tokio::test]
async fn test_concurrent_ttl_expiry() {
    let engine = Arc::new(KvEngine::new(16, Arc::new(ConsolidationManager::disabled())));
    let num_tasks = 64;
    let ops_per_task = 30;

    let mut handles = Vec::new();

    for i in 0..num_tasks {
        let e = Arc::clone(&engine);

        handles.push(tokio::spawn(async move {
            for j in 0..ops_per_task {
                // Set some keys with 1ms TTL (will expire very quickly)
                let short_key = format!("ttl:short:{}:{}", i, j);
                e.set(
                    &short_key,
                    format!("ephemeral-{}-{}", i, j).into_bytes(),
                    Some(1), // 1ms TTL
                    MemoryType::Episodic,
                )
                .unwrap();

                // Set some keys with no TTL (persistent)
                let long_key = format!("ttl:long:{}:{}", i, j);
                e.set(
                    &long_key,
                    format!("persistent-{}-{}", i, j).into_bytes(),
                    None,
                    MemoryType::Semantic,
                )
                .unwrap();

                // Try to get the short-lived key — it may or may not be expired
                // This should NOT panic either way
                let _ = e.get(&short_key);

                // The persistent key should always be readable
                let long_val = e.get(&long_key);
                assert!(
                    long_val.is_some(),
                    "persistent key {} should exist",
                    long_key
                );
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Wait for all short-TTL entries to expire
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Reap expired entries concurrently with reads
    let mut reap_handles = Vec::new();
    for _ in 0..10 {
        let e = Arc::clone(&engine);
        reap_handles.push(tokio::spawn(async move {
            // Concurrent reap
            let _reaped = e.reap_all_expired().await;
            // Concurrent reads on persistent keys
            for i in 0..10 {
                let key = format!("ttl:long:{}:0", i);
                let val = e.get(&key);
                assert!(val.is_some(), "persistent key should survive reap");
            }
        }));
    }

    for handle in reap_handles {
        handle.await.unwrap();
    }

    // All short-TTL keys should be gone after reaping
    let short_entries = engine.scan_prefix("ttl:short:");
    assert!(
        short_entries.is_empty(),
        "all short-TTL entries should be expired, found {}",
        short_entries.len()
    );

    // Long keys should still all exist
    let long_entries = engine.scan_prefix("ttl:long:");
    assert_eq!(
        long_entries.len(),
        num_tasks * ops_per_task,
        "all persistent entries should remain"
    );
}

/// Stress test: many concurrent set/get/delete across many shards with mixed memory types.
#[tokio::test]
async fn test_concurrent_mixed_memory_types() {
    let engine = Arc::new(KvEngine::new(32, Arc::new(ConsolidationManager::disabled())));
    let num_tasks = 100;

    let mut handles = Vec::new();

    for i in 0..num_tasks {
        let e = Arc::clone(&engine);

        handles.push(tokio::spawn(async move {
            // Episodic
            let epi_key = format!("epi:{}:data", i);
            e.set(
                &epi_key,
                b"episodic".to_vec(),
                Some(600_000),
                MemoryType::Episodic,
            )
            .unwrap();

            // Semantic
            let sem_key = format!("sem:{}:fact", i);
            e.set(&sem_key, b"semantic".to_vec(), None, MemoryType::Semantic)
                .unwrap();

            // Procedural
            let proc_key = format!("proc:{}:skill", i);
            e.set(
                &proc_key,
                b"procedural".to_vec(),
                None,
                MemoryType::Procedural,
            )
            .unwrap();

            // Read them back
            assert_eq!(e.get(&epi_key), Some(b"episodic".to_vec()));
            assert_eq!(e.get(&sem_key), Some(b"semantic".to_vec()));
            assert_eq!(e.get(&proc_key), Some(b"procedural".to_vec()));

            // Delete episodic
            assert!(e.delete(&epi_key));
            assert!(e.get(&epi_key).is_none());

            // Semantic and procedural should survive
            assert!(e.get(&sem_key).is_some());
            assert!(e.get(&proc_key).is_some());
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Verify final state: all episodic deleted, all semantic and procedural present
    let epi = engine.scan_prefix("epi:");
    let sem = engine.scan_prefix("sem:");
    let proc = engine.scan_prefix("proc:");

    assert_eq!(epi.len(), 0, "all episodic keys should be deleted");
    assert_eq!(sem.len(), num_tasks, "all semantic keys should remain");
    assert_eq!(proc.len(), num_tasks, "all procedural keys should remain");
}

/// Test that concurrent reaping doesn't cause data corruption or panics.
#[tokio::test]
async fn test_concurrent_reap_and_mutate() {
    let engine = Arc::new(KvEngine::new(8, Arc::new(ConsolidationManager::disabled())));

    // Insert a mix of entries: some expiring soon, some persistent
    for i in 0..200 {
        let key = format!("mix:k{}", i);
        if i % 3 == 0 {
            // Short TTL — will expire
            engine
                .set(
                    &key,
                    format!("v{}", i).into_bytes(),
                    Some(1),
                    MemoryType::Episodic,
                )
                .unwrap();
        } else {
            // No TTL — persistent
            engine
                .set(
                    &key,
                    format!("v{}", i).into_bytes(),
                    None,
                    MemoryType::Semantic,
                )
                .unwrap();
        }
    }

    // Wait for short-TTL entries to expire
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let mut handles = Vec::new();

    // Task group 1: reap repeatedly
    for _ in 0..10 {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for _ in 0..20 {
                let _ = e.reap_all_expired().await;
            }
        }));
    }

    // Task group 2: read persistent entries
    for _ in 0..10 {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for i in 0..200 {
                if i % 3 != 0 {
                    // Persistent keys should always be readable
                    let val = e.get(&format!("mix:k{}", i));
                    assert!(val.is_some(), "persistent key mix:k{} should exist", i);
                }
            }
        }));
    }

    // Task group 3: add new entries while reaping
    for t in 0..10 {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for j in 0..50 {
                let key = format!("new:t{}:k{}", t, j);
                e.set(&key, b"new_val".to_vec(), None, MemoryType::Procedural)
                    .unwrap();
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // All persistent original keys should still exist
    for i in 0..200 {
        if i % 3 != 0 {
            assert!(
                engine.get(&format!("mix:k{}", i)).is_some(),
                "persistent key mix:k{} should survive",
                i
            );
        }
    }

    // All new keys should exist
    for t in 0..10 {
        for j in 0..50 {
            assert!(
                engine.get(&format!("new:t{}:k{}", t, j)).is_some(),
                "new key should exist"
            );
        }
    }
}
