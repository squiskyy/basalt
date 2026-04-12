/// io_uring-backed RESP2 server for Linux.
///
/// Alternative RESP server using raw io_uring for maximum throughput.
/// Runs in a dedicated thread with its own io_uring instance, completely
/// separate from the tokio runtime used by the HTTP server.
///
/// Architecture:
/// - Multishot Accept: one SQE returns all new connections
/// - Per-connection state machine: Accept → Read → Process → Write → Read
/// - Pre-allocated buffer pool with per-connection read buffers
/// - Batched write serialization (same pipelining as tokio server)
///
/// Safety: All buffer pointers passed to io_uring are heap-allocated and
/// pinned via Box. Connections are stored in a slab that is pre-allocated
/// to avoid reallocation while operations are in flight.
///
/// Gated behind `--features io-uring`. Requires Linux kernel 5.19+.

use std::collections::VecDeque;
use std::net::TcpListener;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use io_uring::{opcode, squeue, types, IoUring};

use crate::http::auth::AuthStore;
use crate::store::engine::KvEngine;

use super::commands::{check_command_namespace, CommandHandler};
use super::error::RespError;
use super::parser::{parse_pipeline, serialize_pipeline, RespValue};

const RING_SIZE: u32 = 1024;
const READ_BUF_SIZE: usize = 8192;
/// Max connections — slab is pre-allocated to avoid reallocation.
const MAX_CONNECTIONS: usize = 4096;

/// Token types for tracking in-flight io_uring operations.
#[derive(Clone, Debug)]
enum Token {
    Accept,
    Read { fd: i32, conn_id: usize },
    Write { fd: i32, conn_id: usize },
}

/// Per-connection state.
struct Connection {
    /// Accumulated incoming RESP data.
    read_buf: Vec<u8>,
    /// Response data being written (kept alive until write completes).
    write_buf: Option<Vec<u8>>,
    /// Bytes of write_buf already sent.
    write_progress: usize,
    /// Auth state.
    authenticated: bool,
    auth_token: Option<String>,
}

/// Run the io_uring RESP2 server. Blocks the calling thread.
pub fn run(
    host: &str,
    port: u16,
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
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

    // Separate map for read staging buffers — Box<Vec<u8>> gives us stable pointers.
    let mut read_bufs: std::collections::HashMap<usize, Box<[u8; READ_BUF_SIZE]>> =
        std::collections::HashMap::with_capacity(MAX_CONNECTIONS);

    let handler = Arc::new(CommandHandler::new(engine, db_path));
    let auth_enabled = auth.is_enabled();

    // Submit multishot accept
    let accept_token = tokens.insert(Token::Accept);
    let accept_e = opcode::AcceptMulti::new(types::Fd(listener_fd))
        .build()
        .user_data(accept_token as u64);

    unsafe {
        sq.push(&accept_e).expect("SQ full for accept");
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
                            if sq.push(&ae).is_err() {
                                backlog.push_back(ae);
                            }
                        }
                    }
                    Some(Token::Read { fd, conn_id }) => {
                        tracing::debug!("read error fd={fd}: {err}");
                        close_conn(fd, conn_id, token_idx, &mut connections, &mut tokens, &mut read_bufs);
                    }
                    Some(Token::Write { fd, conn_id }) => {
                        tracing::debug!("write error fd={fd}: {err}");
                        close_conn(fd, conn_id, token_idx, &mut connections, &mut tokens, &mut read_bufs);
                    }
                    None => {}
                }
                continue;
            }

            match tokens.get(token_idx).cloned() {
                Some(Token::Accept) => {
                    let fd = result;
                    set_nonblocking(fd);

                    let conn_id = connections.insert(Connection {
                        read_buf: Vec::with_capacity(READ_BUF_SIZE),
                        write_buf: None,
                        write_progress: 0,
                        authenticated: !auth_enabled,
                        auth_token: None,
                    });

                    // Allocate a stable read buffer (Box ensures stable address)
                    let read_buf = Box::new([0u8; READ_BUF_SIZE]);
                    let read_ptr = read_buf.as_ptr() as *mut u8;
                    read_bufs.insert(conn_id, read_buf);

                    let read_token = tokens.insert(Token::Read { fd, conn_id });

                    let recv_e = opcode::Recv::new(types::Fd(fd), read_ptr, READ_BUF_SIZE as _)
                        .build()
                        .user_data(read_token as u64);

                    unsafe {
                        if sq.push(&recv_e).is_err() {
                            backlog.push_back(recv_e);
                        }
                    }
                }

                Some(Token::Read { fd, conn_id }) => {
                    if result == 0 {
                        close_conn(fd, conn_id, token_idx, &mut connections, &mut tokens, &mut read_bufs);
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
                        // No complete command yet — read more
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if sq.push(&recv_e).is_err() {
                                    backlog.push_back(recv_e);
                                }
                            }
                        }
                        continue;
                    }

                    // Process commands
                    let mut responses: Vec<RespValue> = Vec::with_capacity(values.len());
                    for value in values {
                        if let Some(cmd) = value.to_command() {
                            if cmd.name == "AUTH" {
                                let resp = handle_auth_resp(
                                    &cmd, &auth, &mut conn.authenticated, &mut conn.auth_token,
                                );
                                responses.push(resp);
                                continue;
                            }
                        }
                        if auth_enabled && !conn.authenticated {
                            responses.push(RespValue::Error("NOAUTH Authentication required".into()));
                            continue;
                        }
                        match value.to_command() {
                            Some(cmd) => {
                                // Per-command namespace authorization check
                                if let Some(ref token) = conn.auth_token {
                                    if let Err(err_resp) = check_command_namespace(&cmd, &auth, token) {
                                        responses.push(err_resp);
                                        continue;
                                    }
                                }
                                responses.push(handler.handle(&cmd));
                            }
                            None => responses.push(RespValue::Error("ERR invalid command format".into())),
                        }
                    }

                    if !responses.is_empty() {
                        let out = serialize_pipeline(&responses);
                        let out_len = out.len();
                        conn.write_buf = Some(out);
                        conn.write_progress = 0;

                        tokens[token_idx] = Token::Write { fd, conn_id };
                        let ptr = conn.write_buf.as_ref().unwrap().as_ptr();
                        let write_e = opcode::Send::new(types::Fd(fd), ptr, out_len as _)
                            .build()
                            .user_data(token_idx as u64);
                        unsafe {
                            if sq.push(&write_e).is_err() {
                                backlog.push_back(write_e);
                            }
                        }
                    } else {
                        // No response — read more
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if sq.push(&recv_e).is_err() {
                                    backlog.push_back(recv_e);
                                }
                            }
                        }
                    }
                }

                Some(Token::Write { fd, conn_id }) => {
                    let written = result as usize;
                    let conn = match connections.get_mut(conn_id) {
                        Some(c) => c,
                        None => {
                            tokens.remove(token_idx);
                            continue;
                        }
                    };

                    conn.write_progress += written;
                    let total = conn.write_buf.as_ref().map_or(0, |b| b.len());

                    if conn.write_progress < total {
                        // Partial write — send remaining bytes
                        let ptr = unsafe {
                            conn.write_buf.as_ref().unwrap().as_ptr().add(conn.write_progress)
                        };
                        let remaining = total - conn.write_progress;
                        let write_e = opcode::Send::new(types::Fd(fd), ptr, remaining as _)
                            .build()
                            .user_data(token_idx as u64);
                        unsafe {
                            if sq.push(&write_e).is_err() {
                                backlog.push_back(write_e);
                            }
                        }
                    } else {
                        // Write complete — submit another read
                        conn.write_buf = None;
                        conn.write_progress = 0;
                        if let Some(rbuf) = read_bufs.get(&conn_id) {
                            let ptr = rbuf.as_ptr() as *mut u8;
                            tokens[token_idx] = Token::Read { fd, conn_id };
                            let recv_e = opcode::Recv::new(types::Fd(fd), ptr, READ_BUF_SIZE as _)
                                .build()
                                .user_data(token_idx as u64);
                            unsafe {
                                if sq.push(&recv_e).is_err() {
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
            Some(sqe) => unsafe { let _ = sq.push(&sqe); },
            None => break,
        }
    }
    Ok(())
}

fn close_conn(
    fd: i32,
    conn_id: usize,
    token_idx: usize,
    connections: &mut slab::Slab<Connection>,
    tokens: &mut slab::Slab<Token>,
    read_bufs: &mut std::collections::HashMap<usize, Box<[u8; READ_BUF_SIZE]>>,
) {
    if connections.contains(conn_id) {
        connections.remove(conn_id);
    }
    if tokens.contains(token_idx) {
        tokens.remove(token_idx);
    }
    read_bufs.remove(&conn_id);
    unsafe { libc::close(fd); }
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn handle_auth_resp(
    cmd: &super::parser::Command,
    auth: &AuthStore,
    authenticated: &mut bool,
    auth_token: &mut Option<String>,
) -> RespValue {
    if cmd.args.len() != 1 {
        return RespValue::Error("ERR wrong number of arguments for 'AUTH'".to_string());
    }
    let token = String::from_utf8_lossy(&cmd.args[0]).to_string();
    // Check if this token exists at all (valid token).
    // For AUTH, we just verify the token is valid - per-command namespace
    // checks happen on each subsequent command.
    if auth.token_exists(&token) {
        *authenticated = true;
        *auth_token = Some(token);
        RespValue::SimpleString("OK".to_string())
    } else {
        RespValue::Error("ERR invalid token".to_string())
    }
}
