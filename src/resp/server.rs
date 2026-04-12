use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::http::auth::AuthStore;
use crate::replication::{ReplicationRole, ReplicationState};
use crate::store::engine::KvEngine;

use super::commands::{CommandHandler, ReplicaofResult};
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
    let repl_state = Arc::new(ReplicationState::new_primary(engine.clone(), 10_000));
    run_with_replication(host, port, engine, auth, db_path, repl_state).await
}

/// Run the RESP2 TCP server with replication support.
pub async fn run_with_replication(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
    repl_state: Arc<ReplicationState>,
) -> Result<(), RespError> {
    let listener = TcpListener::bind((host, port)).await.map_err(RespError::Bind)?;
    tracing::info!("RESP server listening on {host}:{port}");

    let handler = Arc::new(CommandHandler::with_replication(engine.clone(), db_path, repl_state.clone()));
    let auth_enabled = auth.is_enabled();

    loop {
        let (socket, addr) = listener.accept().await.map_err(RespError::Accept)?;
        let handler = Arc::clone(&handler);
        let auth = Arc::clone(&auth);
        let auth_enabled = auth_enabled;
        let repl_state = repl_state.clone();
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, handler, auth, auth_enabled, repl_state, engine).await {
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
    repl_state: Arc<ReplicationState>,
    engine: Arc<KvEngine>,
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

                    // Check for REPLICAOF command
                    if cmd.name == "REPLICAOF" {
                        let result = handler.handle_replicaof(&cmd);
                        match result {
                            ReplicaofResult::NoOne => {
                                // Stop replicating, become primary again
                                repl_state.stop_replica_stream();
                                repl_state.set_role(ReplicationRole::Primary);
                                tracing::info!("REPLICAOF NO ONE — now primary");
                                responses.push(RespValue::SimpleString("OK".to_string()));
                            }
                            ReplicaofResult::Replicate { host, port } => {
                                // Become a replica
                                let repl_host = host.clone();
                                let repl_port = port;
                                repl_state.stop_replica_stream();
                                repl_state.set_role(ReplicationRole::Replica {
                                    primary_host: host,
                                    primary_port: port,
                                });
                                tracing::info!("REPLICAOF {repl_host}:{repl_port} — becoming replica");

                                // Send OK immediately, then start replication in background
                                responses.push(RespValue::SimpleString("OK".to_string()));

                                // Flush the OK response before starting replication
                                let out = serialize_pipeline(&responses);
                                if let Err(e) = writer.write_all(&out).await {
                                    tracing::error!("failed to write REPLICAOF response: {e}");
                                    return Err(RespError::Write(e));
                                }
                                writer.flush().await.map_err(RespError::Write)?;
                                responses.clear();

                                // Start replication from primary
                                let repl_engine = engine.clone();
                                let repl_repl_state = repl_state.clone();
                                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                                repl_state.set_replica_shutdown(Some(shutdown_tx));

                                tokio::spawn(async move {
                                    match crate::replication::replica::replicate_from_primary(
                                        &repl_host,
                                        repl_port,
                                        &repl_engine,
                                        &repl_repl_state,
                                        shutdown_rx,
                                    ).await {
                                        Ok(()) => {
                                            tracing::info!("replication from {repl_host}:{repl_port} ended cleanly");
                                        }
                                        Err(e) => {
                                            tracing::error!("replication error from {repl_host}:{repl_port}: {e}");
                                        }
                                    }
                                });

                                // Continue processing commands
                                continue;
                            }
                            ReplicaofResult::Error(msg) => {
                                responses.push(RespValue::Error(msg));
                            }
                        }
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
