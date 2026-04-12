use std::error::Error;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::store::engine::KvEngine;

use super::commands::CommandHandler;
use super::parser::{parse_pipeline, serialize};

/// Run the RESP2 TCP server.
///
/// Binds to `host:port`, accepts connections, and spawns a task per connection.
/// Supports RESP pipelining: multiple commands in a single read are all processed.
pub async fn run(host: &str, port: u16, engine: Arc<KvEngine>) -> Result<(), Box<dyn Error>> {
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
    socket: TcpStream,
    handler: Arc<CommandHandler>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;

    let mut stream = socket;
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            // Connection closed by client
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);

        // Process all complete RESP values in the buffer (pipelining support)
        loop {
            let (values, consumed) = parse_pipeline(&buf);
            if values.is_empty() {
                break;
            }

            // Remove consumed bytes from the buffer
            buf.drain(..consumed);

            // Dispatch each command and collect responses
            let mut responses = Vec::with_capacity(values.len());
            for value in values {
                match value.to_command() {
                    Some(cmd) => {
                        let resp = handler.handle(&cmd);
                        responses.push(serialize(&resp));
                    }
                    None => {
                        let err = serialize(&super::parser::RespValue::Error(
                            "ERR invalid command format".to_string(),
                        ));
                        responses.push(err);
                    }
                }
            }

            // Write all responses at once for efficiency
            if !responses.is_empty() {
                let mut out = Vec::new();
                for r in responses {
                    out.extend_from_slice(&r);
                }
                stream.write_all(&out).await?;
                stream.flush().await?;
            }
        }
    }
}
