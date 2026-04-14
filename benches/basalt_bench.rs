#![allow(unused_must_use)]
use basalt::store::engine::KvEngine;
use basalt::store::memory_type::MemoryType;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::sync::Arc;

fn make_engine(shards: usize) -> KvEngine {
    KvEngine::new(
        shards,
        Arc::new(basalt::store::ConsolidationManager::disabled()),
    )
}

fn bench_set(c: &mut Criterion) {
    let engine = make_engine(64);
    let mut group = c.benchmark_group("set");

    for size in [16, 128, 1024, 8192].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("value_size", size), size, |b, &size| {
            let value = vec![b'x'; size];
            let mut i = 0u64;
            b.iter(|| {
                let key = format!("bench:set:{}", i);
                engine.set(
                    black_box(&key),
                    black_box(value.clone()),
                    None,
                    MemoryType::Semantic,
                );
                i += 1;
            });
        });
    }
    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let engine = make_engine(64);

    // Pre-populate 100K keys
    for i in 0..100_000 {
        let key = format!("bench:get:{}", i);
        let value = format!("value:{}", i);
        engine.set(&key, value.into_bytes(), None, MemoryType::Semantic);
    }

    let mut group = c.benchmark_group("get");
    group.bench_function("100k_keys", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("bench:get:{}", i % 100_000);
            let _ = engine.get(black_box(&key));
            i += 1;
        });
    });
    group.finish();
}

fn bench_mixed_workload(c: &mut Criterion) {
    let engine = make_engine(64);

    // Pre-populate 50K keys
    for i in 0..50_000 {
        let key = format!("bench:mix:{}", i);
        let value = format!("value:{}", i);
        engine.set(&key, value.into_bytes(), None, MemoryType::Semantic);
    }

    let mut group = c.benchmark_group("mixed_90_10");
    group.bench_function("read_heavy", |b| {
        let mut i = 0u64;
        b.iter(|| {
            if i.is_multiple_of(10) {
                // 10% writes
                let key = format!("bench:write:{}", i);
                engine.set(
                    black_box(&key),
                    b"new_value".to_vec(),
                    None,
                    MemoryType::Episodic,
                );
            } else {
                // 90% reads
                let key = format!("bench:mix:{}", i % 50_000);
                let _ = engine.get(black_box(&key));
            }
            i += 1;
        });
    });
    group.finish();
}

fn bench_namespace_scan(c: &mut Criterion) {
    let engine = make_engine(64);

    // 10 agents, 1000 memories each
    for agent in 0..10u64 {
        for mem in 0..1000u64 {
            let key = format!("agent-{}:mem:{}", agent, mem);
            engine.set(
                &key,
                b"memory data".to_vec(),
                Some(3600000),
                MemoryType::Episodic,
            );
        }
    }

    let mut group = c.benchmark_group("namespace_scan");
    group.bench_function("scan_single_agent", |b| {
        b.iter(|| {
            let results = engine.scan_prefix(black_box("agent-5:"));
            black_box(&results);
        });
    });
    group.finish();
}

fn bench_set_with_memory_type(c: &mut Criterion) {
    let engine = make_engine(64);
    let mut group = c.benchmark_group("set_with_type");

    group.bench_function("episodic", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("bench:epi:{}", i);
            engine.set(
                black_box(&key),
                b"observation data".to_vec(),
                None,
                MemoryType::Episodic,
            );
            i += 1;
        });
    });

    group.bench_function("semantic", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("bench:sem:{}", i);
            engine.set(
                black_box(&key),
                b"fact data".to_vec(),
                None,
                MemoryType::Semantic,
            );
            i += 1;
        });
    });

    group.bench_function("procedural", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("bench:proc:{}", i);
            engine.set(
                black_box(&key),
                b"skill data".to_vec(),
                None,
                MemoryType::Procedural,
            );
            i += 1;
        });
    });

    group.finish();
}

fn bench_delete_prefix(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete_prefix");

    group.bench_function("100_keys", |b| {
        b.iter_batched(
            || {
                let engine = make_engine(64);
                for i in 0..100 {
                    let key = format!("bench:del:{}", i);
                    engine.set(&key, b"data".to_vec(), None, MemoryType::Semantic);
                }
                engine
            },
            |engine| {
                let count = engine.delete_prefix(black_box("bench:del:"));
                black_box(count);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_set,
    bench_get,
    bench_mixed_workload,
    bench_namespace_scan,
    bench_set_with_memory_type,
    bench_delete_prefix,
);
criterion_main!(benches);
