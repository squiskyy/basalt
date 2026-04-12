use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::http::auth::AuthStore;
use crate::store::engine::KvEngine;

use super::commands::CommandHandler;
use super::error::RespError;
use super::parser::{parse_pipeline, serialize_pipeline, RespValue};

/// Run the RESP2 TCP server.
///
/// Binds to `host:port`, accepts connections, and spawns a task per connection.
/// Supports RESP pipelining: multiple commands in a single read are all processed.
/// Uses buffer recycling and batched writes for maximum throughput.
///
/// If auth is enabled (tokens configured), clients must AUTH before any command.
pub async fn run(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
) -> Result<(), RespError> {
    let listener = TcpListener::bind((host, port)).await.map_err(RespError::Bind)?;
    tracing::info!("RESP server listening on {host}:{port}");

    let handler = Arc::new(CommandHandler::new(engine, db_path));
    let auth_enabled = auth.is_enabled();

    loop {
        let (socket, addr) = listener.accept().await.map_err(RespError::Accept)?;
        let handler = Arc::clone(&handler);
        let auth = Arc::clone(&auth);
        let auth_enabled = auth_enabled;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, handler, auth, auth_enabled).await {
                tracing::debug!("connection {addr} error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    handler: Arc<CommandHandler>,
    auth: Arc<AuthStore>,
    auth_enabled: bool,
) -> Result<(), RespError> {
    let (mut reader, mut writer) = stream.into_split();

    // Recyclable read buffer — grows to fit workload but doesn't shrink
    let mut read_buf = Vec::with_capacity(8192);
    // Reusable staging area for partial reads
    let mut tmp = [0u8; 8192];

    // Track auth state for this connection
    let mut authenticated = !auth_enabled; // If auth not required, skip auth
    let mut auth_token: Option<String> = None;

    loop {
        let n = reader.read(&mut tmp).await.map_err(RespError::Read)?;
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
                // Check for AUTH command first (always allowed)
                if let Some(cmd) = value.to_command() {
                    if cmd.name == "AUTH" {
                        let resp = handle_auth(&cmd, &auth, &mut authenticated, &mut auth_token);
                        responses.push(resp);
                        continue;
                    }
                }

                // If auth is required and not authenticated, reject
                if auth_enabled && !authenticated {
                    responses.push(RespValue::Error(
                        "NOAUTH Authentication required".to_string(),
                    ));
                    continue;
                }

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
                writer.write_all(&out).await.map_err(RespError::Write)?;
                writer.flush().await.map_err(RespError::Write)?;
            }
        }

        // Keep read_buf from growing unbounded — trim if it got large but is now empty
        if read_buf.is_empty() && read_buf.capacity() > 65536 {
            read_buf = Vec::with_capacity(8192);
        }
    }
}

/// Handle AUTH command (Redis-compatible).
/// AUTH <token>
fn handle_auth(
    cmd: &super::parser::Command,
    auth: &AuthStore,
    authenticated: &mut bool,
    auth_token: &mut Option<String>,
) -> RespValue {
    if cmd.args.len() != 1 {
        return RespValue::Error("ERR wrong number of arguments for 'AUTH'".to_string());
    }

    let token = String::from_utf8_lossy(&cmd.args[0]).to_string();

    // Check if this token exists at all (with wildcard access)
    if auth.is_authorized(&token, "*") || auth.list_tokens().iter().any(|(t, _)| t == &token) {
        *authenticated = true;
        *auth_token = Some(token);
        RespValue::SimpleString("OK".to_string())
    } else {
        RespValue::Error("ERR invalid token".to_string())
    }
}
