/// io_uring-backed RESP2 server for Linux.
///
/// Alternative RESP server using raw io_uring for maximum throughput.
/// Runs in a dedicated thread with its own io_uring instance, completely
/// separate from the tokio runtime used by the HTTP server.
///
/// Architecture:
/// - Multishot Accept: one SQE returns all new connections
/// - Per-connection state machine: Accept -> Read -> Process -> Write -> Read
/// - Pre-allocated buffer pool with per-connection read buffers
/// - Batched write serialization (same pipelining as tokio server)
///
/// Safety: All buffer pointers passed to io_uring are heap-allocated and
/// pinned via Box. Connections are stored in a slab that is pre-allocated
/// to MAX_CONNECTIONS to avoid reallocation. Write buffers are stored
/// in a separate HashMap so that slab reallocations cannot invalidate
/// pointers to in-flight write data.
///
/// Gated behind `--features io-uring`. Requires Linux kernel 5.19+.
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::TcpListener;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use io_uring::{IoUring, opcode, squeue, types};

use crate::http::auth::AuthStore;
use crate::replication::{ReplicationRole, ReplicationState};
use crate::store::engine::KvEngine;
use crate::store::share::ShareStore;

use super::commands::{CommandHandler, ShareHandler};
use super::error::RespError;
use super::parser::{RespValue, parse_pipeline, serialize_pipeline};
use super::session::{ClientSession, SessionAction};

const RING_SIZE: u32 = 1024;
const READ_BUF_SIZE: usize = 8192;
/// Max connections - slab is pre-allocated to avoid reallocation.
/// Connections beyond this limit are rejected to prevent slab reallocation
/// which would invalidate pointers referenced by in-flight io_uring ops.
const MAX_CONNECTIONS: usize = 1024;

/// Token types for tracking in-flight io_uring operations.
#[derive(Clone, Debug)]
enum Token {
    Accept,
    Read { fd: i32, conn_id: usize },
    Write { fd: i32, conn_id: usize },
}

/// Per-connection state. Write buffers are stored separately in a HashMap
/// to guarantee stable addresses even if the slab reallocates.
struct Connection {
    /// Accumulated incoming RESP data.
    read_buf: Vec<u8>,
    /// Session state (auth, replica, etc.).
    session: ClientSession,
}

/// Run the io_uring RESP2 server. Blocks the calling thread.
pub fn run(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
    repl_state: Arc<ReplicationState>,
) -> Result<(), RespError> {
    let listener = TcpListener::bind((host, port)).map_err(RespError::Bind)?;
    let listener_fd = listener.as_raw_fd();
    tracing::info!("io_uring RESP server listening on {host}:{port}");

    let mut ring = IoUring::new(RING_SIZE).map_err(RespError::Submit)?;
    let (submitter, mut sq, mut cq) = ring.split();

    // Pre-allocate slab to MAX_CONNECTIONS to prevent reallocation.
    // This ensures stable references while io_uring ops are in flight.
    let mut connections: slab::Slab<Connection> = slab::Slab::with_capacity(MAX_CONNECTIONS);
    let mut tokens: slab::Slab<Token> = slab::Slab::with_capacity(MAX_CONNECTIONS * 2);
    let mut backlog: VecDeque<squeue::Entry> = VecDeque::with_capacity(256);

    // Separate map for read staging buffers - Box ensures stable pointers.
    let mut read_bufs: HashMap<usize, Box<[u8; READ_BUF_SIZE]>> =
        HashMap::with_capacity(MAX_CONNECTIONS);

    // Separate map for write buffers - ensures stable pointers for in-flight
    // io_uring write operations, independent of slab reallocation.
    // This prevents use-after-free if the slab ever reallocates despite
    // pre-allocation (defence in depth).
    let mut write_bufs: HashMap<usize, Vec<u8>> = HashMap::with_capacity(MAX_CONNECTIONS);
    let mut write_progress: HashMap<usize, usize> = HashMap::with_capacity(MAX_CONNECTIONS);

    let handler = Arc::new(CommandHandler::with_replication(
        engine.clone(),
        db_path,
        repl_state.clone(),
    ));
    let share_handler = Arc::new(ShareHandler::new(share.clone(), auth.clone()));
    let auth_enabled = auth.is_enabled();

    // Submit multishot accept
    let accept_token = tokens.insert(Token::Accept);
    let accept_e = opcode::AcceptMulti::new(types::Fd(listener_fd))
        .build()
        .user_data(accept_token as u64);

    unsafe {
        if let Err(e) = sq.push(&accept_e) {
            tracing::warn!("io_uring SQ full on initial accept, queuing to backlog: {e:?}");
            backlog.push_back(accept_e);
        }
    }
    sq.sync();
    submitter.submit().map_err(RespError::Submit)?;

    loop {
        match submitter.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => return Err(RespError::Submit(e)),
        }
        cq.sync();

        drain_backlog(&mut backlog, &mut sq, &submitter)?;

        for cqe in &mut cq {
            let result = cqe.result();
            let token_idx = cqe.user_data() as usize;

            if result < 0 {
                let err = std::io::Error::from_raw_os_error(-result);
                match tokens.get(token_idx).cloned() {
                    Some(Token::Accept) => {
                        tracing::error!("io_uring accept error: {err}");
                        let ae = opcode::AcceptMulti::new(types::Fd(listener_fd))
                            .build()
                            .user_data(token_idx as u64);
                        unsafe {
                            if let Err(e) = sq.push(&ae) {
                                tracing::warn!(
                                    "io_uring SQ full on accept re-arm, queuing to backlog: {e:?}"
                                );
                                backlog.push_back(ae);
                            }
                        }
                    }
                    Some(Token::Read { fd, conn_id }) => {
                        tracing::debug!("read error fd={fd}: {err}");
                        close_conn(
                            fd,
                            conn_id,
                            token_idx,
                            &mut connections,
                            &mut tokens,
                            &mut read_bufs,
                            &mut write_bufs,
                            &mut write_progress,
                        );
                    }
                    Some(Token::Write { fd, conn_id }) => {
                        tracing::debug!("write error fd={fd}: {err}");
                        close_conn(
                            fd,
                            conn_id,
                            token_idx,
                            &mut connections,
                            &mut tokens,
                            &mut read_bufs,
                            &mut write_bufs,
                            &mut write_progress,
                        );
                    }
                    None => {}
                }
                continue;
            }

            match tokens.get(token_idx).cloned() {
                Some(Token::Accept) => {
                    let fd = result;
                    set_nonblocking(fd);

                    // Reject connections beyond MAX_CONNECTIONS to prevent
                    // slab reallocation which would cause use-after-free for
                    // in-flight io_uring operations.
                    if connections.len() >= MAX_CONNECTIONS {
                        tracing::warn!(
                            "rejecting connection fd={fd}: at MAX_CONNECTIONS ({MAX_CONNECTIONS})"
                        );
                        unsafe {
                            libc::close(fd);
                        }
                        continue;
                    }

                    let conn_id = connections.insert(Connection {
                        read_buf: Vec::with_capacity(READ_BUF_SIZE),
                        session: ClientSession::with_rate_limit(
                            auth.clone(),
                            share.clone(),
                            auth_enabled,
                            0,
                            1000,
                        ),
                    });

                    // Allocate a stable read buffer (Box ensures stable address)
                    let read_buf = Box::new([0u8; READ_BUF_SIZE]);
                    let read_ptr = read_buf.as_ptr() as *mut u8;
                    read_bufs.insert(conn_id, read_buf);

                    // Initialize write state in separate HashMaps
                    write_bufs.insert(conn_id, Vec::new());
                    write_progress.insert(conn_id, 0);

                    let read_token = tokens.insert(Token::Read { fd, conn_id });

                    let recv_e = opcode::Recv::new(types::Fd(fd), read_ptr, READ_BUF_SIZE as _)
                        .build()
                        .user_data(read_token as u64);

                    unsafe {
                        if let Err(e) = sq.push(&recv_e) {
                            tracing::warn!(
                                "io_uring SQ full on new-conn recv, queuing to backlog: {e:?}"
                            );
                            backlog.push_back(recv_e);
                        }
                    }
                }

                Some(Token::Read { fd, conn_id }) => {
                    if result == 0 {
                        close_conn(
                            fd,
                            conn_id,
                            token_idx,
                            &mut connections,
                            &mut tokens,
                            &mut read_bufs,
                            &mut write_bufs,
                            &mut write_progress,
                        );
                        continue;
                    }

                    let read_len = result as usize;

                    // Copy from the stable read buffer into the connection's accumulating read_buf
                    let conn = match connections.get_mut(conn_id) {
                        Some(c) => c,
                        None => {
                            tokens.remove(token_idx);
                            continue;
                        }
                    };

                    if let Some(rbuf) = read_bufs.get(&conn_id) {
                        conn.read_buf.extend_from_slice(&rbuf[..read_len]);
                    }

                    // Parse RESP commands
                    let (values, consumed) = parse_pipeline(&conn.read_buf);
                    if consumed > 0 {
                        conn.read_buf.drain(..consumed);
                    }
                    if conn.read_buf.is_empty() && conn.read_buf.capacity() > 65536 {
                        conn.read_buf = Vec::with_capacity(READ_BUF_SIZE);
                    }

                    if values.is_empty() {
                        // No complete command yet - read more
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if let Err(e) = sq.push(&recv_e) {
                                    tracing::warn!(
                                        "io_uring SQ full on read-rearm (no cmd), queuing to backlog: {e:?}"
                                    );
                                    backlog.push_back(recv_e);
                                }
                            }
                        }
                        continue;
                    }

                    // Process commands - delegate to shared session
                    let mut result =
                        conn.session
                            .process_command_batch(&values, &handler, &share_handler);

                    // Handle REPLICAOF actions
                    match result.action {
                        SessionAction::ReplicaofNoOne => {
                            repl_state.stop_replica_stream();
                            repl_state.set_role(ReplicationRole::Primary);
                            conn.session.set_replica(false);
                            tracing::info!("REPLICAOF NO ONE - now primary (io_uring)");
                        }
                        SessionAction::ReplicaofReplicate { ref host, port } => {
                            // io_uring mode cannot spawn async replication tasks
                            // since it runs in its own thread outside tokio.
                            // Replace the OK response with an error.
                            tracing::warn!("REPLICAOF {host}:{port} rejected in io_uring mode");
                            if let Some(last) = result.responses.last_mut() {
                                *last = RespValue::Error(
                                    "ERR REPLICAOF not supported in io_uring mode; use the tokio RESP server for replication".to_string(),
                                );
                            }
                        }
                        SessionAction::None => {}
                    }

                    let responses = result.responses;

                    if !responses.is_empty() {
                        let out = serialize_pipeline(&responses);
                        let out_len = out.len();
                        // Store write buffer in the separate HashMap for stable address
                        write_bufs.insert(conn_id, out);
                        write_progress.insert(conn_id, 0);

                        tokens[token_idx] = Token::Write { fd, conn_id };
                        // Get pointer from the separate write_bufs HashMap
                        let ptr = write_bufs.get(&conn_id).unwrap().as_ptr();
                        let write_e = opcode::Send::new(types::Fd(fd), ptr, out_len as _)
                            .build()
                            .user_data(token_idx as u64);
                        unsafe {
                            if let Err(e) = sq.push(&write_e) {
                                tracing::warn!(
                                    "io_uring SQ full on write submit, queuing to backlog: {e:?}"
                                );
                                backlog.push_back(write_e);
                            }
                        }
                    } else {
                        // No response - read more
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if let Err(e) = sq.push(&recv_e) {
                                    tracing::warn!(
                                        "io_uring SQ full on read-after-write, queuing to backlog: {e:?}"
                                    );
                                    backlog.push_back(recv_e);
                                }
                            }
                        }
                    }
                }

                Some(Token::Write { fd, conn_id }) => {
                    let written = result as usize;
                    let conn_exists = connections.contains(conn_id);
                    if !conn_exists {
                        tokens.remove(token_idx);
                        continue;
                    }

                    let progress = write_progress.get(&conn_id).copied().unwrap_or(0) + written;
                    write_progress.insert(conn_id, progress);
                    let total = write_bufs.get(&conn_id).map_or(0, |b| b.len());

                    if progress < total {
                        // Partial write - send remaining bytes using stable write_buf pointer
                        let ptr =
                            unsafe { write_bufs.get(&conn_id).unwrap().as_ptr().add(progress) };
                        let remaining = total - progress;
                        let write_e = opcode::Send::new(types::Fd(fd), ptr, remaining as _)
                            .build()
                            .user_data(token_idx as u64);
                        unsafe {
                            if let Err(e) = sq.push(&write_e) {
                                tracing::warn!(
                                    "io_uring SQ full on partial-write, queuing to backlog: {e:?}"
                                );
                                backlog.push_back(write_e);
                            }
                        }
                    } else {
                        // Write complete - clear write buffer and submit another read
                        write_bufs.insert(conn_id, Vec::new());
                        write_progress.insert(conn_id, 0);
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if let Err(e) = sq.push(&recv_e) {
                                    tracing::warn!(
                                        "io_uring SQ full on read-after-completion, queuing to backlog: {e:?}"
                                    );
                                    backlog.push_back(recv_e);
                                }
                            }
                        }
                    }
                }

                None => {
                    tracing::warn!("io_uring completion for unknown token {token_idx}");
                }
            }
        }

        drain_backlog(&mut backlog, &mut sq, &submitter)?;
    }
}

fn drain_backlog(
    backlog: &mut VecDeque<squeue::Entry>,
    sq: &mut io_uring::SubmissionQueue,
    submitter: &io_uring::Submitter,
) -> Result<(), RespError> {
    loop {
        sq.sync();
        if sq.is_full() {
            match submitter.submit() {
                Ok(_) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => break,
                Err(e) => return Err(RespError::Submit(e)),
            }
            sq.sync();
        }
        match backlog.pop_front() {
            Some(sqe) => unsafe {
                if let Err(e) = sq.push(&sqe) {
                    tracing::warn!(
                        "io_uring SQ still full in drain_backlog, re-queuing SQE: {e:?}"
                    );
                    backlog.push_front(sqe);
                    break;
                }
            },
            None => break,
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn close_conn(
    fd: i32,
    conn_id: usize,
    token_idx: usize,
    connections: &mut slab::Slab<Connection>,
    tokens: &mut slab::Slab<Token>,
    read_bufs: &mut HashMap<usize, Box<[u8; READ_BUF_SIZE]>>,
    write_bufs: &mut HashMap<usize, Vec<u8>>,
    write_progress: &mut HashMap<usize, usize>,
) {
    if connections.contains(conn_id) {
        connections.remove(conn_id);
    }
    if tokens.contains(token_idx) {
        tokens.remove(token_idx);
    }
    read_bufs.remove(&conn_id);
    write_bufs.remove(&conn_id);
    write_progress.remove(&conn_id);
    unsafe {
        libc::close(fd);
    }
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
