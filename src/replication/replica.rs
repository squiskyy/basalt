/// Replica-side replication logic.
///
/// When a node becomes a replica:
/// 1. Connects to the primary's RESP port
/// 2. Sends REPLCONF handshake (optional)
/// 3. Receives +FULLRESYNC <seq>\r\n
/// 4. Receives snapshot data
/// 5. Receives +STREAM\r\n
/// 6. Applies WAL entries as they arrive

use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::replication::wal;
use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;

/// Connect to a primary and perform full resync + stream.
/// This runs until the connection is lost, the shutdown signal fires,
/// or an error occurs.
pub async fn replicate_from_primary(
    primary_host: &str,
    primary_port: u16,
    engine: &Arc<KvEngine>,
    repl_state: &Arc<crate::replication::ReplicationState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let addr = format!("{}:{}", primary_host, primary_port);
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("connect to primary {addr}: {e}"))?;

    tracing::info!("connected to primary at {addr}");

    let (mut reader, _writer) = stream.split();

    // Read +FULLRESYNC <seq>\r\n
    let current_seq = read_full_resync(&mut reader).await?;
    tracing::info!("received FULLRESYNC with seq {current_seq}");

    // Read snapshot entries
    let snapshot_count = read_snapshot_count(&mut reader).await?;
    tracing::info!("receiving {snapshot_count} snapshot entries from primary");

    for i in 0..snapshot_count {
        let entry = read_snapshot_entry(&mut reader).await?;
        // Apply to local engine using set_force
        // Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
        let ttl = if entry.ttl_ms == 0 {
            None
        } else {
            Some(entry.ttl_ms)
        };
        engine.set_force_with_embedding(&entry.key, entry.value, ttl, entry.mem_type, entry.embedding);
        if (i + 1) % 1000 == 0 {
            tracing::debug!("restored {}/{} snapshot entries", i + 1, snapshot_count);
        }
    }
    tracing::info!("restored {snapshot_count} snapshot entries from primary");

    // Read +STREAM\r\n
    read_stream_marker(&mut reader).await?;
    tracing::info!("entering streaming mode from primary");

    // Now stream WAL entries
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    loop {
        tokio::select! {
            result = reader.read(&mut tmp) => {
                match result {
                    Ok(0) => {
                        tracing::info!("primary connection closed");
                        return Err("primary connection closed".to_string());
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        // Try to parse and apply WAL entries
                        loop {
                            // We expect BulkString WAL entries
                            // Try to parse one RESP value
                            let (values, consumed) = crate::resp::parser::parse_pipeline(&buf);
                            if values.is_empty() || consumed == 0 {
                                break;
                            }
                            buf.drain(..consumed);

                            for value in values {
                                if let Some(entry_data) = extract_bulk_string(&value) {
                                    match wal::deserialize_entry(&entry_data) {
                                        Ok((_seq, entry, _consumed)) => {
                                            apply_wal_entry(engine, &entry);
                                            repl_state.set_replication_offset(_seq);
                                        }
                                        Err(e) => {
                                            tracing::warn!("failed to deserialize WAL entry: {e}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        return Err(format!("read error from primary: {e}"));
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("replica shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Read the +FULLRESYNC <seq> line from primary.
async fn read_full_resync(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<u64, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return Err("connection closed while reading FULLRESYNC".to_string()),
            Ok(_) => {
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    let line_str = String::from_utf8_lossy(&line[..line.len() - 2]);
                    if let Some(seq_str) = line_str.strip_prefix("+FULLRESYNC ") {
                        return seq_str
                            .trim()
                            .parse::<u64>()
                            .map_err(|e| format!("invalid FULLRESYNC seq: {e}"));
                    } else {
                        return Err(format!("expected +FULLRESYNC, got: {line_str}"));
                    }
                }
            }
            Err(e) => return Err(format!("read FULLRESYNC: {e}")),
        }
    }
}

/// Read the snapshot entry count (RESP Integer).
async fn read_snapshot_count(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<usize, String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return Err("connection closed while reading snapshot count".to_string()),
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n") {
                    let line_str = String::from_utf8_lossy(&buf);
                    if line_str.starts_with(':') {
                        let num_str = &line_str[1..line_str.len() - 2]; // trim :\r\n
                        return num_str
                            .trim()
                            .parse::<usize>()
                            .map_err(|e| format!("invalid snapshot count: {e}"));
                    } else {
                        return Err(format!("expected integer for snapshot count, got: {line_str}"));
                    }
                }
            }
            Err(e) => return Err(format!("read snapshot count: {e}")),
        }
    }
}

/// A snapshot entry received from primary.
struct SnapshotEntry {
    key: String,
    value: Vec<u8>,
    mem_type: MemoryType,
    ttl_ms: u64,
    embedding: Option<Vec<f32>>,
}

/// Read a single snapshot entry (RESP Array).
async fn read_snapshot_entry(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<SnapshotEntry, String> {
    // We need to parse a RESP Array: [SET, key, value, mem_type, ttl_ms]
    // Read enough data to parse a full RESP value
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return Err("connection closed while reading snapshot entry".to_string()),
            Ok(_) => {
                buf.push(byte[0]);
                // Try to parse
                let (values, consumed) = crate::resp::parser::parse_pipeline(&buf);
                if !values.is_empty() {
                    if consumed < buf.len() {
                        // Extra data, shouldn't happen in normal flow but handle it
                    }
                    let value = values.into_iter().next().unwrap();
                    return parse_snapshot_entry(value);
                }
                // Otherwise keep reading
            }
            Err(e) => return Err(format!("read snapshot entry: {e}")),
        }
    }
}

/// Parse a RESP Array into a SnapshotEntry.
fn parse_snapshot_entry(value: crate::resp::parser::RespValue) -> Result<SnapshotEntry, String> {
    let arr = match value {
        crate::resp::parser::RespValue::Array(Some(arr)) => arr,
        _ => return Err("expected RESP Array for snapshot entry".to_string()),
    };

    if arr.len() < 5 || arr.len() > 6 {
        return Err(format!("expected 5 or 6 elements in snapshot entry, got {}", arr.len()));
    }

    // Element 0: "SET"
    let _cmd = extract_bulk_string_or_err(&arr[0], "command")?;
    // Element 1: key
    let key_bytes = extract_bulk_string_or_err(&arr[1], "key")?;
    let key = String::from_utf8(key_bytes).map_err(|e| format!("invalid key UTF-8: {e}"))?;
    // Element 2: value
    let value = extract_bulk_string_or_err(&arr[2], "value")?;
    // Element 3: memory type
    let mt_str = extract_bulk_string_or_err(&arr[3], "mem_type")?;
    let mt_str = String::from_utf8_lossy(&mt_str).to_string();
    let mem_type = match mt_str.to_lowercase().as_str() {
        "episodic" => MemoryType::Episodic,
        "semantic" => MemoryType::Semantic,
        "procedural" => MemoryType::Procedural,
        _ => return Err(format!("invalid memory type: {mt_str}")),
    };
    // Element 4: ttl_ms
    // Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
    // Parse as u64 directly to avoid sign-extension issues from i64.
    let ttl_str = extract_bulk_string_or_err(&arr[4], "ttl")?;
    let ttl_str = String::from_utf8_lossy(&ttl_str).to_string();
    let ttl_ms: u64 = ttl_str
        .trim()
        .parse()
        .map_err(|e| format!("invalid ttl_ms: {e}"))?;

    // Element 5: optional embedding (JSON array of f32)
    let embedding = if arr.len() == 6 {
        let emb_bytes = extract_bulk_string_or_err(&arr[5], "embedding")?;
        let emb_str = String::from_utf8_lossy(&emb_bytes).to_string();
        if emb_str.is_empty() || emb_str == "null" {
            None
        } else {
            // Parse as JSON array of f32
            match serde_json::from_str::<Vec<f32>>(&emb_str) {
                Ok(v) => Some(v),
                Err(_) => None,
            }
        }
    } else {
        None
    };

    Ok(SnapshotEntry {
        key,
        value,
        mem_type,
        ttl_ms,
        embedding,
    })
}

fn extract_bulk_string_or_err(val: &crate::resp::parser::RespValue, name: &str) -> Result<Vec<u8>, String> {
    match val {
        crate::resp::parser::RespValue::BulkString(Some(data)) => Ok(data.clone()),
        _ => Err(format!("expected BulkString for {name}")),
    }
}

/// Read the +STREAM marker from primary.
async fn read_stream_marker(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<(), String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return Err("connection closed while reading STREAM marker".to_string()),
            Ok(_) => {
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    let line_str = String::from_utf8_lossy(&line[..line.len() - 2]);
                    if line_str == "+STREAM" {
                        return Ok(());
                    } else {
                        return Err(format!("expected +STREAM, got: {line_str}"));
                    }
                }
            }
            Err(e) => return Err(format!("read STREAM marker: {e}")),
        }
    }
}

/// Extract BulkString data from a RespValue, if it's a BulkString.
fn extract_bulk_string(val: &crate::resp::parser::RespValue) -> Option<Vec<u8>> {
    match val {
        crate::resp::parser::RespValue::BulkString(Some(data)) => Some(data.clone()),
        _ => None,
    }
}

/// Apply a WAL entry to the local engine.
/// Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
fn apply_wal_entry(engine: &Arc<KvEngine>, entry: &wal::WalEntry) {
    match entry.op {
        wal::WalOp::Set => {
            let ttl = if entry.ttl_ms == 0 { None } else { Some(entry.ttl_ms) };
            let key_str = String::from_utf8_lossy(&entry.key).to_string();
            // Use set_force_with_embedding to preserve vectors for vector search
            engine.set_force_with_embedding(
                &key_str,
                entry.value.clone(),
                ttl,
                entry.mem_type,
                entry.embedding.clone(),
            );
        }
        wal::WalOp::Delete => {
            engine.delete(&String::from_utf8_lossy(&entry.key));
        }
        wal::WalOp::DeletePrefix => {
            engine.delete_prefix(&String::from_utf8_lossy(&entry.key));
        }
    }
}
