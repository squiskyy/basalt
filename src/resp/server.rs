use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::http::auth::AuthStore;
use crate::replication::{ReplicationRole, ReplicationState};
use crate::store::engine::KvEngine;
use crate::store::share::ShareStore;

use super::commands::{CommandHandler, ShareHandler};
use super::error::RespError;
use super::parser::{parse_pipeline, serialize_pipeline};
use super::session::{ClientSession, SessionAction};

#[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
use super::tls::{TlsAcceptor, TlsStream};

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
    share: Arc<ShareStore>,
    db_path: Option<String>,
) -> Result<(), RespError> {
    let repl_state = Arc::new(ReplicationState::new_primary(engine.clone(), 10_000));
    let (_tx, rx) = watch::channel(false);
    run_with_replication(host, port, engine, auth, share, db_path, repl_state, rx).await
}

/// Run the RESP2 TCP server with replication support.
/// Accepts a shutdown receiver; when the shutdown signal fires, the accept
/// loop exits and the server returns gracefully.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_replication(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
    repl_state: Arc<ReplicationState>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), RespError> {
    run_with_replication_and_tls(
        host,
        port,
        engine,
        auth,
        share,
        db_path,
        repl_state,
        shutdown_rx,
        None,
        0,
        1000,
    )
    .await
}

/// Run the RESP2 TCP server with replication and optional TLS support.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_replication_and_tls(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
    repl_state: Arc<ReplicationState>,
    mut shutdown_rx: watch::Receiver<bool>,
    #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))] tls_acceptor: Option<
        TlsAcceptor,
    >,
    #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))] tls_acceptor: Option<()>,
    rate_limit_requests: u64,
    rate_limit_window_ms: u64,
) -> Result<(), RespError> {
    let listener = TcpListener::bind((host, port))
        .await
        .map_err(RespError::Bind)?;

    let tls_enabled = tls_acceptor.is_some();
    if tls_enabled {
        tracing::info!("RESP server listening on {host}:{port} (TLS enabled)");
    } else {
        tracing::info!("RESP server listening on {host}:{port} (plaintext)");
    }

    let handler = Arc::new(CommandHandler::with_replication(
        engine.clone(),
        db_path.clone(),
        repl_state.clone(),
    ));
    let share_handler = Arc::new(ShareHandler::new(share.clone(), auth.clone()));
    let auth_enabled = auth.is_enabled();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (socket, addr) = result.map_err(RespError::Accept)?;
                let handler = Arc::clone(&handler);
                let share_handler = Arc::clone(&share_handler);
                let auth = Arc::clone(&auth);
                let share = share.clone();
                let repl_state = repl_state.clone();
                let engine = engine.clone();

                #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
                let tls_acceptor = tls_acceptor.clone();

                tokio::spawn(async move {
                    #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
                    if let Some(ref acceptor) = tls_acceptor {
                        match acceptor.accept(socket).await {
                            Ok(tls_stream) => {
                                if let Err(e) = handle_connection_tls(
                                    tls_stream, handler, share_handler, auth, auth_enabled, share, repl_state, engine,
                                    rate_limit_requests, rate_limit_window_ms,
                                ).await {
                                    tracing::debug!("TLS connection {addr} error: {e}");
                                }
                            }
                            Err(e) => {
                                tracing::warn!("TLS handshake failed for {addr}: {e}");
                            }
                        }
                    } else {
                        if let Err(e) = handle_connection(
                            socket, handler, share_handler, auth, auth_enabled, share, repl_state, engine,
                            rate_limit_requests, rate_limit_window_ms,
                        ).await {
                            tracing::debug!("connection {addr} error: {e}");
                        }
                    }

                    #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
                    if let Err(e) = handle_connection(
                        socket, handler, share_handler, auth, auth_enabled, share, repl_state, engine,
                        rate_limit_requests, rate_limit_window_ms,
                    ).await {
                        tracing::debug!("connection {addr} error: {e}");
                    }
                });
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("RESP server shutting down gracefully");
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: TcpStream,
    handler: Arc<CommandHandler>,
    share_handler: Arc<ShareHandler>,
    auth: Arc<AuthStore>,
    auth_enabled: bool,
    share: Arc<ShareStore>,
    repl_state: Arc<ReplicationState>,
    engine: Arc<KvEngine>,
    rate_limit_requests: u64,
    rate_limit_window_ms: u64,
) -> Result<(), RespError> {
    let (mut reader, mut writer) = stream.into_split();

    // Recyclable read buffer - grows to fit workload but doesn't shrink
    let mut read_buf = Vec::with_capacity(8192);
    // Reusable staging area for partial reads
    let mut tmp = [0u8; 8192];

    // Per-connection session managing auth state and command dispatch
    let mut session = ClientSession::with_rate_limit(
        auth.clone(),
        share.clone(),
        auth_enabled,
        rate_limit_requests,
        rate_limit_window_ms,
    );

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

            // Delegate auth/dispatch to ClientSession
            let result = session.process_command_batch(&values, &handler, &share_handler);

            // Handle REPLICAOF actions that require server-level coordination
            let responses_already_flushed = match result.action {
                SessionAction::ReplicaofNoOne => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Primary);
                    tracing::info!("REPLICAOF NO ONE - now primary");
                    false
                }
                SessionAction::ReplicaofReplicate { host, port } => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Replica {
                        primary_host: host.clone(),
                        primary_port: port,
                    });
                    tracing::info!("REPLICAOF {host}:{port} - becoming replica");

                    // Flush responses before spawning replication task
                    let out = serialize_pipeline(&result.responses);
                    if let Err(e) = writer.write_all(&out).await {
                        tracing::error!("failed to write REPLICAOF response: {e}");
                        return Err(RespError::Write(e));
                    }
                    writer.flush().await.map_err(RespError::Write)?;

                    // Spawn replication task
                    let repl_engine = engine.clone();
                    let repl_repl_state = repl_state.clone();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                    repl_state.set_replica_shutdown(Some(shutdown_tx));

                    tokio::spawn(async move {
                        match crate::replication::replica::replicate_from_primary(
                            &host,
                            port,
                            &repl_engine,
                            &repl_repl_state,
                            shutdown_rx,
                        )
                        .await
                        {
                            Ok(()) => {
                                tracing::info!("replication from {host}:{port} ended cleanly");
                            }
                            Err(e) => {
                                tracing::error!("replication error from {host}:{port}: {e}");
                            }
                        }
                    });

                    true // responses already flushed
                }
                SessionAction::None => false,
            };

            // Batch-serialize all responses into one write - reduces syscalls
            if !responses_already_flushed && !result.responses.is_empty() {
                let out = serialize_pipeline(&result.responses);
                writer.write_all(&out).await.map_err(RespError::Write)?;
                writer.flush().await.map_err(RespError::Write)?;
            }
        }

        // Keep read_buf from growing unbounded - trim if it got large but is now empty
        if read_buf.is_empty() && read_buf.capacity() > 65536 {
            read_buf = Vec::with_capacity(8192);
        }
    }
}

/// Handle a TLS-wrapped RESP connection.
/// Same logic as handle_connection but operates on TlsStream instead of split TcpStream.
#[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
#[allow(clippy::too_many_arguments)]
async fn handle_connection_tls(
    stream: TlsStream,
    handler: Arc<CommandHandler>,
    share_handler: Arc<ShareHandler>,
    auth: Arc<AuthStore>,
    auth_enabled: bool,
    share: Arc<ShareStore>,
    repl_state: Arc<ReplicationState>,
    engine: Arc<KvEngine>,
    rate_limit_requests: u64,
    rate_limit_window_ms: u64,
) -> Result<(), RespError> {
    // TlsStream implements AsyncRead + AsyncWrite, but we can't split it
    // like TcpStream. Instead, we use a single-stream approach with
    // a shared buffer and careful read/write interleaving.
    // For simplicity and correctness, we wrap in a split-like approach
    // using tokio::io::split which requires Unpin.
    let (mut reader, mut writer) = tokio::io::split(stream);

    let mut read_buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    let mut session = ClientSession::with_rate_limit(
        auth.clone(),
        share.clone(),
        auth_enabled,
        rate_limit_requests,
        rate_limit_window_ms,
    );

    loop {
        let n = reader.read(&mut tmp).await.map_err(RespError::Read)?;
        if n == 0 {
            return Ok(());
        }
        read_buf.extend_from_slice(&tmp[..n]);

        loop {
            let (values, consumed) = parse_pipeline(&read_buf);
            if values.is_empty() {
                break;
            }

            read_buf.drain(..consumed);

            let result = session.process_command_batch(&values, &handler, &share_handler);

            let responses_already_flushed = match result.action {
                SessionAction::ReplicaofNoOne => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Primary);
                    tracing::info!("REPLICAOF NO ONE - now primary");
                    false
                }
                SessionAction::ReplicaofReplicate { host, port } => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Replica {
                        primary_host: host.clone(),
                        primary_port: port,
                    });
                    tracing::info!("REPLICAOF {host}:{port} - becoming replica");

                    let out = serialize_pipeline(&result.responses);
                    if let Err(e) = writer.write_all(&out).await {
                        tracing::error!("failed to write REPLICAOF response: {e}");
                        return Err(RespError::Write(e));
                    }
                    writer.flush().await.map_err(RespError::Write)?;

                    let repl_engine = engine.clone();
                    let repl_repl_state = repl_state.clone();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                    repl_state.set_replica_shutdown(Some(shutdown_tx));

                    tokio::spawn(async move {
                        match crate::replication::replica::replicate_from_primary(
                            &host,
                            port,
                            &repl_engine,
                            &repl_repl_state,
                            shutdown_rx,
                        )
                        .await
                        {
                            Ok(()) => {
                                tracing::info!("replication from {host}:{port} ended cleanly");
                            }
                            Err(e) => {
                                tracing::error!("replication error from {host}:{port}: {e}");
                            }
                        }
                    });

                    true
                }
                SessionAction::None => false,
            };

            if !responses_already_flushed && !result.responses.is_empty() {
                let out = serialize_pipeline(&result.responses);
                writer.write_all(&out).await.map_err(RespError::Write)?;
                writer.flush().await.map_err(RespError::Write)?;
            }
        }

        if read_buf.is_empty() && read_buf.capacity() > 65536 {
            read_buf = Vec::with_capacity(8192);
        }
    }
}
