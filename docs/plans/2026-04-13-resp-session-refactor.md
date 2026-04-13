# RESP Session Refactor — Fix Implementation Drift (Issue #47)

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Eliminate duplicated command dispatch logic between `server.rs` and `uring_server.rs` by extracting shared session handling into a common module, then add a shared integration test suite that runs against both backends.

**Architecture:** Create `src/resp/session.rs` with a `ClientSession` struct that owns per-connection auth state and a `process_command_batch()` method. Both server files delegate command processing to this shared module. Each server retains only its I/O loop. Then create `tests/resp_session_test.rs` that parameterizes the existing tokio integration tests and adds a parallel suite for the io_uring backend (behind `#[cfg(feature = "io-uring")]`).

**Tech Stack:** Rust, tokio (standard server), io_uring crate (uring server), existing test infrastructure.

---

## Key Decisions

1. **REPLICAOF handling:** The io_uring server cannot spawn async replication tasks. `process_command_batch()` returns a `ReplicaofAction` enum: the tokio server interprets `ReplicaofAction::Replicate { host, port }` to spawn a replication task; the uring server maps it to an error response. This avoids any async dependency in the shared module.

2. **No async in session.rs:** The shared module is purely synchronous. It takes parsed `RespValue`s, returns `RespValue`s and action enums. All I/O (writing responses, spawning tasks) stays in the server files.

3. **Test approach:** The existing `tests/resp_integration_test.rs` stays as-is (tokio backend). New shared tests in `tests/resp_session_test.rs` test `ClientSession` and `process_command_batch()` directly (no server needed). Then a `tests/resp_uring_integration_test.rs` (io-uring feature gated) runs the same integration tests against the uring backend by starting it in a thread and connecting via TCP.

---

## Task 1: Create `session.rs` with `ClientSession` and `process_command_batch`

**Objective:** Extract the shared command dispatch logic into a new module.

**Files:**
- Create: `src/resp/session.rs`
- Modify: `src/resp/mod.rs`

**Step 1: Create `src/resp/session.rs`**

```rust
//! Shared RESP session logic — eliminates drift between tokio and io_uring servers.
//!
//! Both server backends delegate command dispatch to `ClientSession`, ensuring
//! identical auth, namespace, and REPLICAOF handling. Each server retains only
//! its transport-specific I/O code.

use std::sync::Arc;

use crate::http::auth::AuthStore;
use crate::replication::{ReplicationRole, ReplicationState};
use crate::store::engine::KvEngine;

use super::commands::{CommandHandler, ReplicaofResult, check_command_namespace};
use super::parser::{Command, RespValue};

/// Per-connection state for auth and session tracking.
pub struct ClientSession {
    /// Whether this connection has authenticated successfully.
    authenticated: bool,
    /// The auth token provided by the client (if authenticated).
    auth_token: Option<String>,
    /// Whether auth is required on this server.
    auth_enabled: bool,
    /// Shared auth store for token validation and namespace checks.
    auth: Arc<AuthStore>,
    /// Whether this connection is in replica mode (io_uring tracking).
    is_replica: bool,
}

/// Action returned by `process_command_batch` that the server must handle
/// outside the shared dispatch logic (e.g., spawning async replication).
pub enum SessionAction {
    /// Client sent REPLICAOF NO ONE — stop replication and become primary.
    ReplicaofNoOne,
    /// Client sent REPLICAOF host port — start replicating from that primary.
    /// The tokio server spawns an async task; the uring server rejects this.
    ReplicaofReplicate { host: String, port: u16 },
    /// No special action needed — responses are in the returned vec.
    None,
}

/// Result of processing a batch of RESP commands.
pub struct BatchResult {
    /// RESP responses to write back to the client.
    pub responses: Vec<RespValue>,
    /// Any action the server needs to take beyond writing responses.
    pub action: SessionAction,
}

impl ClientSession {
    /// Create a new session. If `auth_enabled` is false, the session starts
    /// pre-authenticated.
    pub fn new(auth: Arc<AuthStore>, auth_enabled: bool) -> Self {
        ClientSession {
            authenticated: !auth_enabled,
            auth_token: None,
            auth_enabled,
            auth,
            is_replica: false,
        }
    }

    /// Whether this session has authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// The auth token, if any.
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    /// Whether this connection is in replica mode.
    pub fn is_replica(&self) -> bool {
        self.is_replica
    }

    /// Set replica mode (used by server after processing a REPLICAOF NO ONE).
    pub fn set_replica(&mut self, value: bool) {
        self.is_replica = value;
    }

    /// Process a batch of parsed RESP values. Returns responses and any
    /// server-level action (like starting replication).
    ///
    /// This is the single source of truth for command dispatch, auth checks,
    /// and namespace enforcement. Both tokio and io_uring servers call this.
    pub fn process_command_batch(
        &mut self,
        values: &[RespValue],
        handler: &CommandHandler,
    ) -> BatchResult {
        let mut responses: Vec<RespValue> = Vec::with_capacity(values.len());
        let mut action = SessionAction::None;

        for value in values {
            if let Some(cmd) = value.to_command() {
                // AUTH is always allowed, even without authentication
                if cmd.name == "AUTH" {
                    let resp = handle_auth(&cmd, &self.auth, &mut self.authenticated, &mut self.auth_token);
                    responses.push(resp);
                    continue;
                }

                // REPLICAOF requires special handling at the server level
                if cmd.name == "REPLICAOF" {
                    let result = handler.handle_replicaof(&cmd);
                    match result {
                        ReplicaofResult::NoOne => {
                            action = SessionAction::ReplicaofNoOne;
                            responses.push(RespValue::SimpleString("OK".to_string()));
                        }
                        ReplicaofResult::Replicate { host, port } => {
                            action = SessionAction::ReplicaofReplicate { host, port };
                            // Note: the server decides whether to accept or reject this.
                            // The tokio server will write OK and spawn replication.
                            // The uring server will replace the OK with an error.
                            responses.push(RespValue::SimpleString("OK".to_string()));
                        }
                        ReplicaofResult::Error(msg) => {
                            responses.push(RespValue::Error(msg));
                        }
                    }
                    continue;
                }
            }

            // If auth is required and not authenticated, reject
            if self.auth_enabled && !self.authenticated {
                responses.push(RespValue::Error(
                    "NOAUTH Authentication required".to_string(),
                ));
                continue;
            }

            match value.to_command() {
                Some(cmd) => {
                    // Per-command namespace authorization check
                    if let Some(ref token) = self.auth_token
                        && let Err(err_resp) = check_command_namespace(&cmd, &self.auth, token)
                    {
                        responses.push(err_resp);
                        continue;
                    }
                    responses.push(handler.handle(&cmd));
                }
                None => {
                    responses.push(RespValue::Error("ERR invalid command format".to_string()));
                }
            }
        }

        BatchResult { responses, action }
    }
}

/// Handle AUTH command (Redis-compatible).
/// AUTH <token>
fn handle_auth(
    cmd: &Command,
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
```

**Step 2: Add `session` to `src/resp/mod.rs`**

Add `pub mod session;` after the existing module declarations:

```rust
pub mod commands;
pub mod error;
pub mod parser;
pub mod session;
pub mod server;

#[cfg(feature = "io-uring")]
pub mod uring_server;

pub use error::RespError;
```

**Step 3: Verify it compiles**

Run: `cargo check 2>&1`
Expected: compiles with no errors (session.rs is self-contained, not yet used)

**Step 4: Commit**

```bash
git add src/resp/session.rs src/resp/mod.rs
git commit -m "feat(resp): add ClientSession and process_command_batch (issue #47)"
```

---

## Task 2: Refactor `server.rs` to use `ClientSession`

**Objective:** Replace the inline auth/dispatch code in the tokio server with `ClientSession`.

**Files:**
- Modify: `src/resp/server.rs`

**Step 1: Replace `handle_connection` with `ClientSession`**

Replace the `handle_connection` function body and remove the `handle_auth` function. The new `handle_connection`:

```rust
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

    // Shared session state — eliminates drift with uring_server
    let mut session = super::session::ClientSession::new(auth.clone(), auth_enabled);

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

            // Delegate all command dispatch to the shared session
            let result = session.process_command_batch(&values, &handler);

            // Handle REPLICAOF action
            match result.action {
                super::session::SessionAction::ReplicaofNoOne => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Primary);
                    tracing::info!("REPLICAOF NO ONE — now primary");
                }
                super::session::SessionAction::ReplicaofReplicate { host, port } => {
                    repl_state.stop_replica_stream();
                    repl_state.set_role(ReplicationRole::Primary {
                        primary_host: host.clone(),
                        primary_port: port,
                    });
                    tracing::info!("REPLICAOF {host}:{port} — becoming replica");

                    // Flush the OK response before starting replication
                    let out = serialize_pipeline(&result.responses);
                    if let Err(e) = writer.write_all(&out).await {
                        tracing::error!("failed to write REPLICAOF response: {e}");
                        return Err(RespError::Write(e));
                    }
                    writer.flush().await.map_err(RespError::Write)?;

                    // Start replication from primary
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
                                tracing::info!(
                                    "replication from {host}:{port} ended cleanly"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "replication error from {host}:{port}: {e}"
                                );
                            }
                        }
                    });

                    // Responses already flushed — continue processing
                    continue;
                }
                super::session::SessionAction::None => {}
            }

            // Batch-serialize all responses into one write — reduces syscalls
            if !result.responses.is_empty() {
                let out = serialize_pipeline(&result.responses);
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
```

**Step 2: Remove the old `handle_auth` function** (it's now in `session.rs`).

**Step 3: Remove unused imports** — `AuthStore` is still needed for `ClientSession::new`, but `check_command_namespace`, `ReplicaofResult`, and `Command`/`RespValue` imports that were only used in the inline dispatch can be cleaned up. The `use super::commands::{CommandHandler, ReplicaofResult, check_command_namespace};` line becomes `use super::commands::CommandHandler;` since `ReplicaofResult` and `check_command_namespace` are no longer directly used.

Update imports to:

```rust
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::http::auth::AuthStore;
use crate::replication::{ReplicationRole, ReplicationState};
use crate::store::engine::KvEngine;

use super::commands::CommandHandler;
use super::error::RespError;
use super::parser::{RespValue, parse_pipeline, serialize_pipeline};
use super::session::ClientSession;
```

**Step 4: Run tests**

Run: `cargo test 2>&1`
Expected: all tests pass (tokio server behavior unchanged)

**Step 5: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1`
Expected: no warnings

**Step 6: Commit**

```bash
git add src/resp/server.rs
git commit -m "refactor(resp): server.rs uses ClientSession from session.rs (issue #47)"
```

---

## Task 3: Refactor `uring_server.rs` to use `ClientSession`

**Objective:** Replace the inline auth/dispatch code in the io_uring server with `ClientSession`.

**Files:**
- Modify: `src/resp/uring_server.rs`

**Step 1: Update imports**

Replace:
```rust
use super::commands::{CommandHandler, ReplicaofResult, check_command_namespace};
use super::parser::{RespValue, parse_pipeline, serialize_pipeline};
```

With:
```rust
use super::commands::CommandHandler;
use super::parser::{parse_pipeline, serialize_pipeline};
use super::session::{ClientSession, SessionAction};
```

Remove the `RespValue` import if no longer used directly (it is still used for constructing error responses in the REPLICAOF-rejection path, so keep it).

**Step 2: Update the `Connection` struct**

Replace:
```rust
struct Connection {
    read_buf: Vec<u8>,
    authenticated: bool,
    auth_token: Option<String>,
    is_replica: bool,
}
```

With:
```rust
struct Connection {
    read_buf: Vec<u8>,
    session: ClientSession,
}
```

**Step 3: Update connection creation**

In the `Token::Accept` arm, replace:
```rust
let conn_id = connections.insert(Connection {
    read_buf: Vec::with_capacity(READ_BUF_SIZE),
    authenticated: !auth_enabled,
    auth_token: None,
    is_replica: false,
});
```

With:
```rust
let conn_id = connections.insert(Connection {
    read_buf: Vec::with_capacity(READ_BUF_SIZE),
    session: ClientSession::new(auth.clone(), auth_enabled),
});
```

**Step 4: Replace the inline command dispatch in the `Token::Read` arm**

Replace the entire block from `// Process commands` through the end of the `for value in values` loop and the `if !responses.is_empty()` / `else` block with:

```rust
                    // Process commands — delegate to shared session
                    let result = conn.session.process_command_batch(&values, &handler);

                    // Handle REPLICAOF action
                    match result.action {
                        SessionAction::ReplicaofNoOne => {
                            repl_state.stop_replica_stream();
                            repl_state.set_role(ReplicationRole::Primary);
                            conn.session.set_replica(false);
                            tracing::info!("REPLICAOF NO ONE - now primary (io_uring)");
                        }
                        SessionAction::ReplicaofReplicate { host, port } => {
                            // io_uring mode cannot spawn async replication tasks
                            // since it runs in its own thread outside tokio.
                            // Replace the OK response with an error.
                            tracing::warn!(
                                "REPLICAOF {host}:{port} rejected in io_uring mode"
                            );
                            // The batch returned OK as the last response.
                            // Replace it with the error.
                            if let Some(last) = result.responses.last_mut() {
                                *last = RespValue::Error(
                                    "ERR REPLICAOF not supported in io_uring mode; use the tokio RESP server for replication".to_string(),
                                );
                            }
                        }
                        SessionAction::None => {}
                    }

                    if !result.responses.is_empty() {
                        let out = serialize_pipeline(&result.responses);
                        let out_len = out.len();
                        write_bufs.insert(conn_id, out);
                        write_progress.insert(conn_id, 0);

                        tokens[token_idx] = Token::Write { fd, conn_id };
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
```

**Step 5: Remove the old `handle_auth_resp` function** from the bottom of `uring_server.rs` (it's now in `session.rs`).

**Step 6: Verify compilation**

Run: `cargo check --features io-uring 2>&1`
Expected: compiles with no errors

**Step 7: Run clippy**

Run: `cargo clippy --all-targets --features io-uring -- -D warnings 2>&1`
Expected: no warnings

**Step 8: Commit**

```bash
git add src/resp/uring_server.rs
git commit -m "refactor(resp): uring_server.rs uses ClientSession from session.rs (issue #47)"
```

---

## Task 4: Add unit tests for `ClientSession`

**Objective:** Test the shared session logic directly without starting any server.

**Files:**
- Create: `tests/session_test.rs`

**Step 1: Write tests**

```rust
//! Unit tests for ClientSession — shared RESP session logic.
//!
//! These tests exercise auth, namespace enforcement, REPLICAOF handling,
//! and command dispatch without starting any server.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::resp::commands::CommandHandler;
use basalt::resp::parser::{RespValue, parse_pipeline};
use basalt::resp::session::{ClientSession, SessionAction};
use basalt::store::engine::KvEngine;

/// Helper: encode a RESP command and parse it into a RespValue.
fn make_command(name: &str, args: &[&str]) -> RespValue {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("*{}\r\n", 1 + args.len()).as_bytes());
    buf.extend_from_slice(format!("${}\r\n{name}\r\n", name.len()).as_bytes());
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    let (values, _) = parse_pipeline(&buf);
    assert_eq!(values.len(), 1, "expected exactly one parsed value");
    values.into_iter().next().unwrap()
}

/// Helper: create a handler with a fresh engine.
fn make_handler() -> Arc<CommandHandler> {
    let engine = Arc::new(KvEngine::new(4));
    Arc::new(CommandHandler::new(engine, None))
}

// ---- No-auth tests ----

#[test]
fn test_no_auth_ping() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();
    let values = vec![make_command("PING", &[])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(result.responses[0], RespValue::SimpleString("PONG".to_string()));
    assert!(matches!(result.action, SessionAction::None));
}

#[test]
fn test_no_auth_set_get() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    let values = vec![make_command("SET", &["mykey", "myval"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("OK".to_string()));

    let values = vec![make_command("GET", &["mykey"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::BulkString(Some(b"myval".to_vec())));
}

// ---- Auth flow tests ----

#[test]
fn test_auth_required_rejects_commands() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "token1".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, true);
    let handler = make_handler();

    let values = vec![make_command("PING", &[])];
    let result = session.process_command_batch(&values, &handler);
    assert!(matches!(&result.responses[0], RespValue::Error(msg) if msg.contains("NOAUTH")));
}

#[test]
fn test_auth_succeeds_then_commands_work() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "token1".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth.clone(), true);
    let handler = make_handler();

    // AUTH
    let values = vec![make_command("AUTH", &["token1"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("OK".to_string()));
    assert!(session.is_authenticated());

    // Now PING should work
    let values = vec![make_command("PING", &[])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("PONG".to_string()));
}

#[test]
fn test_auth_wrong_token() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "token1".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, true);
    let handler = make_handler();

    let values = vec![make_command("AUTH", &["wrong"])];
    let result = session.process_command_batch(&values, &handler);
    assert!(matches!(&result.responses[0], RespValue::Error(msg) if msg.contains("invalid token")));
    assert!(!session.is_authenticated());
}

// ---- Namespace scoping tests ----

#[test]
fn test_scoped_token_can_access_own_namespace() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth.clone(), true);
    let handler = make_handler();

    // AUTH
    let values = vec![make_command("AUTH", &["scoped"])];
    let _ = session.process_command_batch(&values, &handler);

    // SET in allowed namespace
    let values = vec![make_command("SET", &["ns-alpha:mykey", "myval"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("OK".to_string()));
}

#[test]
fn test_scoped_token_cannot_access_other_namespace() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth.clone(), true);
    let handler = make_handler();

    // AUTH
    let values = vec![make_command("AUTH", &["scoped"])];
    let _ = session.process_command_batch(&values, &handler);

    // SET in disallowed namespace
    let values = vec![make_command("SET", &["ns-beta:mykey", "myval"])];
    let result = session.process_command_batch(&values, &handler);
    assert!(matches!(&result.responses[0], RespValue::Error(msg) if msg.contains("NOAUTH")));
}

// ---- REPLICAOF tests ----

#[test]
fn test_replicaof_no_one() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    let values = vec![make_command("REPLICAOF", &["NO", "ONE"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("OK".to_string()));
    assert!(matches!(result.action, SessionAction::ReplicaofNoOne));
}

#[test]
fn test_replicaof_replicate() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    let values = vec![make_command("REPLICAOF", &["127.0.0.1", "6379"])];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses[0], RespValue::SimpleString("OK".to_string()));
    match result.action {
        SessionAction::ReplicaofReplicate { host, port } => {
            assert_eq!(host, "127.0.0.1");
            assert_eq!(port, 6379);
        }
        other => panic!("expected ReplicaofReplicate, got {:?}", action_name(&other)),
    }
}

#[test]
fn test_replicaof_invalid() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    let values = vec![make_command("REPLICAOF", &[])];
    let result = session.process_command_batch(&values, &handler);
    assert!(matches!(&result.responses[0], RespValue::Error(_)));
    assert!(matches!(result.action, SessionAction::None));
}

// ---- Pipelining test ----

#[test]
fn test_pipeline_multiple_commands() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    let values = vec![
        make_command("PING", &[]),
        make_command("SET", &["key1", "val1"]),
        make_command("GET", &["key1"]),
    ];
    let result = session.process_command_batch(&values, &handler);
    assert_eq!(result.responses.len(), 3);
    assert_eq!(result.responses[0], RespValue::SimpleString("PONG".to_string()));
    assert_eq!(result.responses[1], RespValue::SimpleString("OK".to_string()));
    assert_eq!(result.responses[2], RespValue::BulkString(Some(b"val1".to_vec())));
}

// ---- Invalid command test ----

#[test]
fn test_invalid_command_format() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, false);
    let handler = make_handler();

    // A simple string is not a valid command array
    let values = vec![RespValue::SimpleString("NOTACOMMAND".to_string())];
    let result = session.process_command_batch(&values, &handler);
    assert!(matches!(&result.responses[0], RespValue::Error(msg) if msg.contains("invalid command format")));
}

// Helper for debug printing action variants
fn action_name(action: &SessionAction) -> &'static str {
    match action {
        SessionAction::ReplicaofNoOne => "ReplicaofNoOne",
        SessionAction::ReplicaofReplicate { .. } => "ReplicaofReplicate",
        SessionAction::None => "None",
    }
}
```

**Step 2: Run the tests**

Run: `cargo test --test session_test 2>&1`
Expected: all tests pass

**Step 3: Commit**

```bash
git add tests/session_test.rs
git commit -m "test(resp): add ClientSession unit tests (issue #47)"
```

---

## Task 5: Add io_uring integration test suite

**Objective:** Run the same integration test scenarios against the io_uring backend to catch behavioral drift.

**Files:**
- Create: `tests/resp_uring_integration_test.rs`
- Modify: `tests/resp_integration_test.rs` (extract shared helpers into a module)

Since the existing `resp_integration_test.rs` has substantial test helper code (encode/parse/connect/start server), we'll create a shared test helper module that both test files can use.

**Step 1: Create shared test helpers**

Create `tests/common/mod.rs`:

```rust
//! Shared test helpers for RESP integration tests.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::store::engine::KvEngine;

/// Build a RESP-encoded command array: *N\r\n$len\r\nname\r\n...
pub fn encode_resp_command(name: &str, args: &[&str]) -> Vec<u8> {
    let total_args = 1 + args.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("*{total_args}\r\n").as_bytes());
    buf.extend_from_slice(format!("${}\r\n{name}\r\n", name.len()).as_bytes());
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    buf
}

/// Try to parse one complete RESP value from the buffer.
/// Returns the number of bytes consumed if a complete value is found.
pub fn try_parse_resp(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    match buf[0] {
        b'+' | b'-' => find_crlf(buf).map(|pos| pos + 2),
        b':' => find_crlf(buf).map(|pos| pos + 2),
        b'$' => {
            let crlf_pos = find_crlf(buf)?;
            let len_end = crlf_pos + 2;
            let len_str = std::str::from_utf8(&buf[1..crlf_pos]).ok()?;
            let len: i64 = len_str.parse().ok()?;
            if len == -1 {
                return Some(len_end);
            }
            let data_end = len_end + (len as usize) + 2;
            if buf.len() >= data_end {
                Some(data_end)
            } else {
                None
            }
        }
        b'*' => {
            let crlf_pos = find_crlf(buf)?;
            let count_end = crlf_pos + 2;
            let count_str = std::str::from_utf8(&buf[1..crlf_pos]).ok()?;
            let count: i64 = count_str.parse().ok()?;
            if count == -1 {
                return Some(count_end);
            }
            let mut offset = count_end;
            for _ in 0..count {
                let consumed = try_parse_resp(&buf[offset..])?;
                offset += consumed;
            }
            Some(offset)
        }
        _ => None,
    }
}

/// Find the position of \r\n in the buffer.
pub fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Create a default KvEngine for tests.
pub fn make_engine() -> Arc<KvEngine> {
    Arc::new(KvEngine::new(4))
}

/// Create an AuthStore with no tokens (auth disabled).
pub fn make_no_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::new())
}

/// Create an AuthStore with a wildcard token.
pub fn make_wildcard_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::from_list(vec![(
        "bsk-test-token".to_string(),
        vec!["*".to_string()],
    )]))
}

/// Create an AuthStore with scoped tokens for namespace tests.
pub fn make_scoped_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::from_list(vec![
        ("bsk-scoped".to_string(), vec!["ns-alpha".to_string()]),
        ("bsk-wildcard".to_string(), vec!["*".to_string()]),
    ]))
}
```

**Step 2: Refactor `tests/resp_integration_test.rs`** to use the shared helpers

Replace the local helper functions with imports from `common`:

```rust
mod common;

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::resp::server::run as resp_run;
use basalt::store::engine::KvEngine;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{encode_resp_command, find_crlf, make_engine, make_no_auth, make_scoped_auth, make_wildcard_auth, try_parse_resp};

// ... rest of the test file stays the same, but delete the local
// encode_resp_command, try_parse_resp, and find_crlf functions
// and replace start_default_resp_server / start_auth_resp_server
// to use make_engine() / make_wildcard_auth() from common
```

**Step 3: Create `tests/resp_uring_integration_test.rs`**

```rust
//! io_uring RESP integration tests.
//!
//! These tests start the io_uring RESP server on a random port and run
//! the same test scenarios as the tokio integration tests.
//!
//! Gated behind `#[cfg(feature = "io-uring")]` since io_uring requires
//! Linux kernel 5.19+.

#![cfg(feature = "io-uring")]

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use basalt::http::auth::AuthStore;
use basalt::replication::ReplicationState;
use basalt::resp::uring_server;
use basalt::store::engine::KvEngine;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::*;

/// Start the io_uring RESP server on a random port.
/// Returns the port number. The server runs on a background thread.
fn start_uring_server(
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    db_path: Option<String>,
) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    // Small delay for port release
    thread::sleep(Duration::from_millis(10));

    let repl_state = Arc::new(ReplicationState::new_primary(engine.clone(), 10_000));

    thread::Builder::new()
        .name("io-uring-resp-test".into())
        .spawn(move || {
            let _ = uring_server::run("127.0.0.1", port, engine, auth, db_path, repl_state);
        })
        .unwrap();

    // Wait for server to bind
    thread::sleep(Duration::from_millis(200));
    port
}

/// Start uring server with no auth.
fn start_default_uring_server() -> u16 {
    start_uring_server(make_engine(), make_no_auth(), None)
}

/// Start uring server with wildcard auth.
fn start_auth_uring_server() -> u16 {
    start_uring_server(make_engine(), make_wildcard_auth(), None)
}

/// Start uring server with scoped auth.
fn start_scoped_auth_uring_server() -> u16 {
    start_uring_server(make_engine(), make_scoped_auth(), None)
}

async fn connect(port: u16) -> TcpStream {
    TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap()
}

async fn read_resp_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if !buf.is_empty() && let Some(consumed) = try_parse_resp(&buf) {
            return String::from_utf8_lossy(&buf[..consumed]).to_string();
        }
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            panic!("connection closed unexpectedly");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn send_and_read(stream: &mut TcpStream, name: &str, args: &[&str]) -> String {
    let cmd = encode_resp_command(name, args);
    stream.write_all(&cmd).await.unwrap();
    stream.flush().await.unwrap();
    read_resp_response(stream).await
}

// ---- Same test suite as tokio, but against io_uring ----

#[tokio::test]
async fn test_uring_ping_returns_pong() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert_eq!(resp, "+PONG\r\n");
}

#[tokio::test]
async fn test_uring_set_returns_ok() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "SET", &["mykey", "myval"]).await;
    assert_eq!(resp, "+OK\r\n");
}

#[tokio::test]
async fn test_uring_get_returns_bulk_string() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let _ = send_and_read(&mut stream, "SET", &["testkey", "testval"]).await;
    let resp = send_and_read(&mut stream, "GET", &["testkey"]).await;
    assert_eq!(resp, "$7\r\ntestval\r\n");
}

#[tokio::test]
async fn test_uring_get_nonexistent_returns_null() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "GET", &["nonexistent"]).await;
    assert_eq!(resp, "$-1\r\n");
}

#[tokio::test]
async fn test_uring_del_returns_count() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let _ = send_and_read(&mut stream, "SET", &["delkey", "delval"]).await;
    let resp = send_and_read(&mut stream, "DEL", &["delkey"]).await;
    assert_eq!(resp, ":1\r\n");
    let resp = send_and_read(&mut stream, "DEL", &["delkey"]).await;
    assert_eq!(resp, ":0\r\n");
}

#[tokio::test]
async fn test_uring_mset_mget() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "MSET", &["k1", "v1", "k2", "v2"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "MGET", &["k1", "k2"]).await;
    assert_eq!(resp, "*2\r\n$2\r\nv1\r\n$2\r\nv2\r\n");
}

#[tokio::test]
async fn test_uring_info_returns_bulk_string() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "INFO", &[]).await;
    assert!(resp.starts_with('$'), "INFO should be bulk string, got: {resp}");
    assert!(resp.contains("basalt_version"), "INFO should contain basalt_version, got: {resp}");
}

#[tokio::test]
async fn test_uring_auth_flow() {
    let port = start_auth_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert!(resp.starts_with("-NOAUTH"), "Expected NOAUTH, got: {resp}");
    let resp = send_and_read(&mut stream, "AUTH", &["bsk-test-token"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert_eq!(resp, "+PONG\r\n");
}

#[tokio::test]
async fn test_uring_auth_wrong_token() {
    let port = start_auth_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "AUTH", &["wrong-token"]).await;
    assert!(resp.starts_with("-ERR"), "Expected error, got: {resp}");
    assert!(resp.contains("invalid token"), "Error should mention invalid token, got: {resp}");
}

#[tokio::test]
async fn test_uring_scoped_token_can_access_own_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "AUTH", &["bsk-scoped"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "SET", &["ns-alpha:mykey", "myvalue"]).await;
    assert_eq!(resp, "+OK\r\n");
}

#[tokio::test]
async fn test_uring_scoped_token_cannot_access_other_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "AUTH", &["bsk-scoped"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "SET", &["ns-beta:mykey", "myvalue"]).await;
    assert!(resp.starts_with("-NOAUTH"), "Expected NOAUTH, got: {resp}");
}

#[tokio::test]
async fn test_uring_wildcard_token_can_access_any_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "AUTH", &["bsk-wildcard"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "SET", &["ns-beta:mykey", "myvalue"]).await;
    assert_eq!(resp, "+OK\r\n");
}

#[tokio::test]
async fn test_uring_pipelining() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let mut pipeline = Vec::new();
    pipeline.extend(encode_resp_command("PING", &[]));
    pipeline.extend(encode_resp_command("SET", &["foo", "bar"]));
    pipeline.extend(encode_resp_command("GET", &["foo"]));
    stream.write_all(&pipeline).await.unwrap();
    stream.flush().await.unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 { break; }
        buf.extend_from_slice(&tmp[..n]);
        let mut offset = 0;
        let mut count = 0;
        for _ in 0..3 {
            if offset >= buf.len() { break; }
            match try_parse_resp(&buf[offset..]) {
                Some(consumed) => { offset += consumed; count += 1; }
                None => break,
            }
        }
        if count == 3 { break; }
    }
    let response = String::from_utf8_lossy(&buf).to_string();
    assert_eq!(response, "+PONG\r\n+OK\r\n$3\r\nbar\r\n");
}

#[tokio::test]
async fn test_uring_msett_mgett() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "MSETT", &["mem1", "data1", "episodic"]).await;
    assert_eq!(resp, "+OK\r\n");
    let resp = send_and_read(&mut stream, "MGETT", &["mem1"]).await;
    assert!(resp.starts_with("*3\r\n"), "MGETT should return *3 array, got: {resp}");
    assert!(resp.contains("data1"), "MGETT value should contain data1, got: {resp}");
    assert!(resp.contains("episodic"), "MGETT type should contain episodic, got: {resp}");
}

#[tokio::test]
async fn test_uring_replicaof_rejected() {
    // io_uring mode should reject REPLICAOF with host:port
    let port = start_default_uring_server();
    let mut stream = connect(port).await;
    let resp = send_and_read(&mut stream, "REPLICAOF", &["127.0.0.1", "6379"]).await;
    assert!(
        resp.contains("not supported in io_uring mode"),
        "REPLICAOF should be rejected in io_uring mode, got: {resp}"
    );
}
```

**Step 4: Run all tests**

Run: `cargo test --test session_test 2>&1`
Run: `cargo test --test resp_integration_test 2>&1`
Run: `cargo test --features io-uring --test resp_uring_integration_test 2>&1`
Expected: all pass

**Step 5: Run full CI checks**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features io-uring -- -D warnings && cargo test && cargo test --features io-uring`
Expected: all pass

**Step 6: Commit**

```bash
git add tests/common/mod.rs tests/resp_uring_integration_test.rs tests/resp_integration_test.rs
git commit -m "test(resp): add shared test helpers + io_uring integration suite (issue #47)"
```

---

## Task 6: Final cleanup and CI verification

**Objective:** Run the full CI matrix, clean up dead code, and verify everything passes.

**Files:**
- Potentially modify: `src/resp/server.rs`, `src/resp/uring_server.rs` (if clippy finds issues)

**Step 1: Full format check**

Run: `cargo fmt --all -- --check`
Expected: no diffs

**Step 2: Full clippy (both feature sets)**

Run: `cargo clippy --all-targets -- -D warnings 2>&1`
Run: `cargo clippy --all-targets --features io-uring -- -D warnings 2>&1`
Expected: no warnings

**Step 3: Full test suite**

Run: `cargo test --verbose 2>&1`
Run: `cargo test --verbose --features io-uring 2>&1`
Expected: all pass

**Step 4: Push and verify CI**

```bash
git push origin master
```

Check: `gh run list --repo squiskyy/basalt --limit 3`
Wait for all jobs (fmt, clippy, test) to pass.

**Step 5: Close the issue**

```bash
gh issue close 47 --repo squiskyy/basalt --comment "Fixed by extracting shared session logic into \`src/resp/session.rs\` with \`ClientSession\` + \`process_command_batch()\`. Both server.rs and uring_server.rs now delegate to the same code. Added unit tests for the session and a full io_uring integration test suite to catch future drift."
```

---

## Summary of changes

| File | Action | Description |
|------|--------|-------------|
| `src/resp/session.rs` | Create | `ClientSession` struct + `process_command_batch()` + `SessionAction` enum |
| `src/resp/mod.rs` | Modify | Add `pub mod session;` |
| `src/resp/server.rs` | Modify | Use `ClientSession`, remove `handle_auth` |
| `src/resp/uring_server.rs` | Modify | Use `ClientSession`, remove `handle_auth_resp`, replace `Connection.authenticated/auth_token/is_replica` with `Connection.session` |
| `tests/common/mod.rs` | Create | Shared test helpers (encode, parse, auth constructors) |
| `tests/session_test.rs` | Create | Unit tests for `ClientSession` |
| `tests/resp_uring_integration_test.rs` | Create | io_uring integration tests (feature-gated) |
| `tests/resp_integration_test.rs` | Modify | Use shared helpers from `common/mod.rs` |
