use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::store::engine::KvEngine;

use super::commands::CommandHandler;
use super::parser::{parse_pipeline, serialize_pipeline, RespValue};

/// Run the RESP2 TCP server.
///
/// Binds to `host:port`, accepts connections, and spawns a task per connection.
/// Supports RESP pipelining: multiple commands in a single read are all processed.
/// Uses buffer recycling and batched writes for maximum throughput.
pub async fn run(host: &str, port: u16, engine: Arc<KvEngine>) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind((host, port)).await?;
    tracing::info!("RESP server listening on {host}:{port}");

    let handler = Arc::new(CommandHandler::new(engine));

    loop {
        let (socket, addr) = listener.accept().await?;
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, handler).await {
                tracing::debug!("connection {addr} error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    handler: Arc<CommandHandler>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut reader, mut writer) = stream.into_split();

    // Recyclable read buffer — grows to fit workload but doesn't shrink
    let mut read_buf = Vec::with_capacity(8192);
    // Reusable staging area for partial reads
    let mut tmp = [0u8; 8192];

    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            // Connection closed by client
            return Ok(());
        }
        read_buf.extend_from_slice(&tmp[..n]);

        // Process all complete RESP values in the buffer (pipelining)
        loop {
            let (values, consumed) = parse_pipeline(&read_buf);
            if values.is_empty() {
                break;
            }

            // Remove consumed bytes from the read buffer
            read_buf.drain(..consumed);

            // Dispatch each command and collect responses
            let mut responses: Vec<RespValue> = Vec::with_capacity(values.len());
            for value in values {
                match value.to_command() {
                    Some(cmd) => {
                        let resp = handler.handle(&cmd);
                        responses.push(resp);
                    }
                    None => {
                        responses.push(RespValue::Error(
                            "ERR invalid command format".to_string(),
                        ));
                    }
                }
            }

            // Batch-serialize all responses into one write — reduces syscalls
            if !responses.is_empty() {
                let out = serialize_pipeline(&responses);
                writer.write_all(&out).await?;
                writer.flush().await?;
            }
        }

        // Keep read_buf from growing unbounded — trim if it got large but is now empty
        if read_buf.is_empty() && read_buf.capacity() > 65536 {
            read_buf = Vec::with_capacity(8192);
        }
    }
}
