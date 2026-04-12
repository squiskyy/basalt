use basalt::resp::parser::{RespValue, parse_pipeline, serialize};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench_parse_simple_string(c: &mut Criterion) {
    let input = b"+OK\r\n";
    c.bench_function("parse_simple_string", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

fn bench_parse_bulk_string(c: &mut Criterion) {
    let value = "x".repeat(128);
    let input = format!("${}\r\n{}\r\n", value.len(), value);
    let input = input.as_bytes();
    c.bench_function("parse_bulk_string_128b", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

fn bench_parse_set_command(c: &mut Criterion) {
    let input = b"*3\r\n$3\r\nSET\r\n$10\r\nagent:1:mem\r\n$12\r\nhello world!\r\n";
    c.bench_function("parse_set_command", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

fn bench_parse_pipeline(c: &mut Criterion) {
    // 10 SET commands pipelined
    let mut input = Vec::new();
    for i in 0..10u64 {
        let cmd = format!("*3\r\n$3\r\nSET\r\n$8\r\nkey:{:04}\r\n$6\r\nvalue!\r\n", i);
        input.extend_from_slice(cmd.as_bytes());
    }
    c.bench_function("parse_pipeline_10_sets", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(&input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

fn bench_parse_pipeline_100(c: &mut Criterion) {
    // 100 SET commands pipelined
    let mut input = Vec::new();
    for i in 0..100u64 {
        let cmd = format!("*3\r\n$3\r\nSET\r\n$8\r\nkey:{:04}\r\n$6\r\nvalue!\r\n", i);
        input.extend_from_slice(cmd.as_bytes());
    }
    c.bench_function("parse_pipeline_100_sets", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(&input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

fn bench_serialize_response(c: &mut Criterion) {
    let value = RespValue::BulkString(Some(b"hello world".to_vec()));
    c.bench_function("serialize_bulk_string", |b| {
        b.iter(|| {
            black_box(&serialize(black_box(&value)));
        });
    });
}

fn bench_serialize_pipeline(c: &mut Criterion) {
    let values: Vec<RespValue> = (0..10)
        .map(|_| RespValue::SimpleString("OK".to_string()))
        .collect();
    c.bench_function("serialize_pipeline_10_oks", |b| {
        b.iter(|| {
            black_box(&basalt::resp::parser::serialize_pipeline(black_box(
                &values,
            )));
        });
    });
}

fn bench_crlf_scan(c: &mut Criterion) {
    // Directly benchmark the CRLF scanning that memchr accelerates
    let mut input = Vec::new();
    for i in 0..100u64 {
        let cmd = format!("*3\r\n$3\r\nSET\r\n$8\r\nkey:{:04}\r\n$6\r\nvalue!\r\n", i);
        input.extend_from_slice(cmd.as_bytes());
    }
    c.bench_function("crlf_scan_pipeline_100", |b| {
        b.iter(|| {
            let (vals, consumed) = parse_pipeline(black_box(&input));
            black_box(&vals);
            black_box(&consumed);
        });
    });
}

criterion_group!(
    benches,
    bench_parse_simple_string,
    bench_parse_bulk_string,
    bench_parse_set_command,
    bench_parse_pipeline,
    bench_parse_pipeline_100,
    bench_serialize_response,
    bench_serialize_pipeline,
    bench_crlf_scan,
);
criterion_main!(benches);
