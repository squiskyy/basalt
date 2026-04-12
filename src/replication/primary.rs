/// Primary-side replication logic.
///
/// When a replica connects, the primary:
/// 1. Sends +FULLRESYNC <seq>\r\n
/// 2. Sends the current snapshot as RESP bulk arrays
/// 3. Sends +STREAM\r\n
/// 4. Streams new WAL entries as they arrive

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::replication::wal;
use crate::store::engine::KvEngine;
use crate::resp::parser::RespValue;

/// Send a full resync to a replica connection.
///
/// This sends the snapshot and then starts streaming WAL entries.
/// This function blocks (async) until the replica disconnects or an error occurs.
pub async fn send_full_resync(
    stream: &mut TcpStream,
    engine: &Arc<KvEngine>,
    repl_state: &Arc<crate::replication::ReplicationState>,
) -> Result<(), String> {
    let (mut reader, mut writer) = stream.split();

    // 1. Send +FULLRESYNC <seq>\r\n
    let current_seq = repl_state.wal().current_seq();
    let full_resync = format!("+FULLRESYNC {}\r\n", current_seq);
    writer
        .write_all(full_resync.as_bytes())
        .await
        .map_err(|e| format!("write FULLRESYNC: {e}"))?;
    writer.flush().await.map_err(|e| format!("flush FULLRESYNC: {e}"))?;

    // 2. Send snapshot data as a series of RESP arrays
    // Each entry: Array [BulkString("SET"), BulkString(key), BulkString(value), BulkString(mem_type), Integer(ttl_ms), BulkString(embedding_json)]
    // embedding_json is an empty string if no embedding, or a JSON array of f32 values
    let entries = engine.scan_prefix_with_embeddings("");
    let entry_count = entries.len();

    // Send entry count as an integer
    let count_msg = RespValue::Integer(entry_count as i64);
    let count_bytes = crate::resp::parser::serialize(&count_msg);
    writer
        .write_all(&count_bytes)
        .await
        .map_err(|e| format!("write snapshot count: {e}"))?;

    for (key, value, meta) in &entries {
        // Convert expires_at to remaining TTL.
        // Wire format: ttl_ms = 0 means no expiry, ttl_ms > 0 means expire in N ms.
        let ttl_str = match meta.ttl_remaining_ms {
            Some(ms) => ms.to_string(),
            None => "0".to_string(),
        };
        let mt_str = meta.memory_type.to_string();

        // Serialize embedding as JSON array if present
        let emb_str = match &meta.embedding {
            Some(emb) => serde_json::to_string(emb).unwrap_or_default(),
            None => String::new(),
        };

        let entry_msg = RespValue::Array(Some(vec![
            RespValue::BulkString(Some(b"SET".to_vec())),
            RespValue::BulkString(Some(key.as_bytes().to_vec())),
            RespValue::BulkString(Some(value.clone())),
            RespValue::BulkString(Some(mt_str.into_bytes())),
            RespValue::BulkString(Some(ttl_str.into_bytes())),
            RespValue::BulkString(Some(emb_str.into_bytes())),
        ]));
        let entry_bytes = crate::resp::parser::serialize(&entry_msg);
        writer
            .write_all(&entry_bytes)
            .await
            .map_err(|e| format!("write snapshot entry: {e}"))?;
    }

    // 3. Send +STREAM\r\n to indicate we're now streaming
    writer
        .write_all(b"+STREAM\r\n")
        .await
        .map_err(|e| format!("write STREAM: {e}"))?;
    writer.flush().await.map_err(|e| format!("flush STREAM: {e}"))?;

    // 4. Stream WAL entries
    repl_state.inc_connected_replicas();

    let mut last_seq = current_seq;
    let mut read_buf = [0u8; 256];

    loop {
        // Check for new WAL entries
        let entries = repl_state.wal().entries_from(last_seq + 1);
        if !entries.is_empty() {
            for (seq, entry) in &entries {
                let binary = wal::serialize_entry(entry, *seq);
                // Send as RESP BulkString with a special prefix
                // Format: $<len>\r\n<WAL-BINARY>\r\n
                // We wrap the binary WAL entry in a BulkString
                let msg = RespValue::BulkString(Some(binary));
                let msg_bytes = crate::resp::parser::serialize(&msg);
                if let Err(e) = writer.write_all(&msg_bytes).await {
                    tracing::debug!("replica stream write error: {e}");
                    break;
                }
                last_seq = *seq;
            }
            repl_state.set_replication_offset(last_seq);
            if let Err(e) = writer.flush().await {
                tracing::debug!("replica stream flush error: {e}");
                break;
            }
        }

        // Small sleep to avoid busy-polling
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        // Check if the replica has disconnected
        match reader.read(&mut read_buf).await {
            Ok(0) => {
                // Replica disconnected
                tracing::info!("replica disconnected (EOF)");
                break;
            }
            Ok(_) => {
                // Replica sent data — for now we ignore it
                // (a replica might send REPLICAOF or PING)
            }
            Err(e) => {
                tracing::debug!("replica read error: {e}");
                break;
            }
        }
    }

    repl_state.dec_connected_replicas();
    Ok(())
}
