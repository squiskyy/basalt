//! io_uring RESP integration tests for Basalt.
//!
//! These tests mirror the tokio-based RESP integration tests but use the
//! io_uring server backend. The io_uring server runs in a blocking std
//! thread (not tokio), while test clients use tokio::net::TcpStream for
//! async I/O.
//!
//! Gated behind `#![cfg(feature = "io-uring")]` since io_uring requires
//! Linux kernel 5.19+.

#![cfg(feature = "io-uring")]

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use basalt::replication::ReplicationState;
use basalt::resp::uring_server;
use basalt::store::share::ShareStore;

use common::{
    encode_resp_command, make_engine, make_no_auth, make_scoped_auth, make_wildcard_auth,
    try_parse_resp,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a single RESP response from the stream. Reads until we have a
/// complete RESP value by parsing incrementally.
async fn read_resp_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        // Try to parse what we have so far
        if !buf.is_empty()
            && let Some(consumed) = try_parse_resp(&buf)
        {
            let response = String::from_utf8_lossy(&buf[..consumed]).to_string();
            return response;
        }
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            panic!("connection closed unexpectedly");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Start the io_uring RESP server on a random port. Returns the port.
fn start_uring_server(
    engine: Arc<basalt::store::engine::KvEngine>,
    auth: Arc<basalt::http::auth::AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    thread::sleep(Duration::from_millis(10));

    let repl_state = Arc::new(ReplicationState::new_primary(engine.clone(), 10_000));
    thread::Builder::new()
        .name("io-uring-resp-test".into())
        .spawn(move || {
            let _ = uring_server::run("127.0.0.1", port, engine, auth, share, db_path, repl_state);
        })
        .unwrap();

    // Wait for server to bind
    thread::sleep(Duration::from_millis(200));
    port
}

/// Convenience: start an io_uring RESP server with no auth.
fn start_default_uring_server() -> u16 {
    start_uring_server(
        make_engine(),
        make_no_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
}

/// Start an io_uring RESP server with wildcard auth enabled.
fn start_auth_uring_server() -> u16 {
    start_uring_server(
        make_engine(),
        make_wildcard_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
}

/// Start an io_uring RESP server with scoped auth tokens.
fn start_scoped_auth_uring_server() -> u16 {
    start_uring_server(
        make_engine(),
        make_scoped_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
}

/// Connect to the RESP server and return the TcpStream.
async fn connect(port: u16) -> TcpStream {
    TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap()
}

/// Send a RESP command and read the response.
async fn send_and_read(stream: &mut TcpStream, name: &str, args: &[&str]) -> String {
    let cmd = encode_resp_command(name, args);
    stream.write_all(&cmd).await.unwrap();
    stream.flush().await.unwrap();
    read_resp_response(stream).await
}

// ---------------------------------------------------------------------------
// a. PING returns +PONG
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_ping_returns_pong() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert_eq!(resp, "+PONG\r\n");
}

// ---------------------------------------------------------------------------
// b. SET key value returns +OK
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_set_returns_ok() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "SET", &["mykey", "myval"]).await;
    assert_eq!(resp, "+OK\r\n");
}

// ---------------------------------------------------------------------------
// c. GET key returns bulk string value
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_get_returns_bulk_string() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // SET first
    let _ = send_and_read(&mut stream, "SET", &["testkey", "testval"]).await;

    // GET should return the value as a bulk string
    let resp = send_and_read(&mut stream, "GET", &["testkey"]).await;
    assert_eq!(resp, "$7\r\ntestval\r\n");
}

// ---------------------------------------------------------------------------
// d. GET nonexistent key returns $-1\r\n (null bulk string)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_get_nonexistent_returns_null() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "GET", &["nonexistent"]).await;
    assert_eq!(resp, "$-1\r\n");
}

// ---------------------------------------------------------------------------
// e. DEL key returns :1, DEL nonexistent returns :0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_del_returns_count() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // SET a key first
    let _ = send_and_read(&mut stream, "SET", &["delkey", "delval"]).await;

    // DEL existing key returns :1
    let resp = send_and_read(&mut stream, "DEL", &["delkey"]).await;
    assert_eq!(resp, ":1\r\n");

    // DEL nonexistent key returns :0
    let resp = send_and_read(&mut stream, "DEL", &["delkey"]).await;
    assert_eq!(resp, ":0\r\n");
}

// ---------------------------------------------------------------------------
// f. MSET k1 v1 k2 v2 returns +OK, then MGET k1 k2 returns array
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_mset_mget() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // MSET
    let resp = send_and_read(&mut stream, "MSET", &["k1", "v1", "k2", "v2"]).await;
    assert_eq!(resp, "+OK\r\n");

    // MGET should return an array of two bulk strings
    let resp = send_and_read(&mut stream, "MGET", &["k1", "k2"]).await;
    assert_eq!(resp, "*2\r\n$2\r\nv1\r\n$2\r\nv2\r\n");
}

// ---------------------------------------------------------------------------
// g. INFO returns a bulk string with server info
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_info_returns_bulk_string() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "INFO", &[]).await;
    assert!(
        resp.starts_with('$'),
        "INFO response should be a bulk string, got: {resp}"
    );
    assert!(
        resp.contains("basalt_version"),
        "INFO should contain basalt_version, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// h. AUTH flow: with auth enabled, unauthenticated command returns -NOAUTH,
//    then AUTH with valid token succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_auth_flow() {
    let port = start_auth_uring_server();
    let mut stream = connect(port).await;

    // Without auth, any command (except AUTH) should return NOAUTH error
    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert!(
        resp.starts_with("-NOAUTH"),
        "Expected NOAUTH error, got: {resp}"
    );

    // AUTH with valid token should succeed
    let resp = send_and_read(&mut stream, "AUTH", &["bsk-test-token"]).await;
    assert_eq!(resp, "+OK\r\n");

    // After auth, PING should work
    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert_eq!(resp, "+PONG\r\n");
}

// ---------------------------------------------------------------------------
// i. AUTH with wrong token returns error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_auth_wrong_token() {
    let port = start_auth_uring_server();
    let mut stream = connect(port).await;

    // AUTH with an invalid token
    let resp = send_and_read(&mut stream, "AUTH", &["wrong-token"]).await;
    assert!(
        resp.starts_with("-ERR"),
        "Expected error for wrong token, got: {resp}"
    );
    assert!(
        resp.contains("invalid token"),
        "Error should mention invalid token, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// j. AUTH with scoped token: per-command namespace enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_scoped_token_can_access_own_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "AUTH", &["bsk-scoped"]).await;
    assert_eq!(resp, "+OK\r\n", "AUTH with scoped token should succeed");

    // SET a key in the allowed namespace
    let resp = send_and_read(&mut stream, "SET", &["ns-alpha:mykey", "myvalue"]).await;
    assert_eq!(resp, "+OK\r\n", "SET in allowed namespace should succeed");

    // GET from allowed namespace
    let resp = send_and_read(&mut stream, "GET", &["ns-alpha:mykey"]).await;
    assert!(
        resp.contains("myvalue"),
        "GET from allowed namespace should return value, got: {resp}"
    );
}

#[tokio::test]
async fn test_uring_scoped_token_cannot_access_other_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "AUTH", &["bsk-scoped"]).await;
    assert_eq!(resp, "+OK\r\n", "AUTH with scoped token should succeed");

    // SET a key in a disallowed namespace
    let resp = send_and_read(&mut stream, "SET", &["ns-beta:mykey", "myvalue"]).await;
    assert!(
        resp.starts_with("-NOAUTH"),
        "SET in disallowed namespace should fail, got: {resp}"
    );

    // GET from disallowed namespace
    let resp = send_and_read(&mut stream, "GET", &["ns-beta:mykey"]).await;
    assert!(
        resp.starts_with("-NOAUTH"),
        "GET from disallowed namespace should fail, got: {resp}"
    );
}

#[tokio::test]
async fn test_uring_wildcard_token_can_access_any_namespace() {
    let port = start_scoped_auth_uring_server();
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "AUTH", &["bsk-wildcard"]).await;
    assert_eq!(resp, "+OK\r\n", "AUTH with wildcard token should succeed");

    // SET a key in any namespace
    let resp = send_and_read(&mut stream, "SET", &["ns-beta:mykey", "myvalue"]).await;
    assert_eq!(
        resp, "+OK\r\n",
        "SET with wildcard token in any namespace should succeed"
    );

    let resp = send_and_read(&mut stream, "GET", &["ns-beta:mykey"]).await;
    assert!(
        resp.contains("myvalue"),
        "GET with wildcard token should work, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// k. Pipelining: send multiple commands in one write, read all responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_pipelining() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // Pipeline: PING, SET foo bar, GET foo
    let mut pipeline = Vec::new();
    pipeline.extend(encode_resp_command("PING", &[]));
    pipeline.extend(encode_resp_command("SET", &["foo", "bar"]));
    pipeline.extend(encode_resp_command("GET", &["foo"]));

    stream.write_all(&pipeline).await.unwrap();
    stream.flush().await.unwrap();

    // Read all responses
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        // Try to parse 3 complete RESP values
        let mut offset = 0;
        let mut count = 0;
        for _ in 0..3 {
            if offset >= buf.len() {
                break;
            }
            match try_parse_resp(&buf[offset..]) {
                Some(consumed) => {
                    offset += consumed;
                    count += 1;
                }
                None => break,
            }
        }
        if count == 3 {
            break;
        }
    }

    let response = String::from_utf8_lossy(&buf).to_string();

    // Verify the three pipelined responses appear in order
    assert!(
        response.contains("+PONG\r\n"),
        "Pipeline should contain +PONG, got: {response}"
    );
    assert!(
        response.contains("+OK\r\n"),
        "Pipeline should contain +OK, got: {response}"
    );
    assert!(
        response.contains("$3\r\nbar\r\n"),
        "Pipeline should contain $3\\r\\nbar\\r\\n, got: {response}"
    );

    // Verify exact ordering
    assert_eq!(response, "+PONG\r\n+OK\r\n$3\r\nbar\r\n");
}

// ---------------------------------------------------------------------------
// l. MSETT and MGETT commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_msett_mgett() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // MSETT key value type [PX ms]
    let resp = send_and_read(&mut stream, "MSETT", &["mem1", "data1", "episodic"]).await;
    assert_eq!(resp, "+OK\r\n");

    // MGETT key -> returns array [value, type, ttl]
    let resp = send_and_read(&mut stream, "MGETT", &["mem1"]).await;
    assert!(
        resp.starts_with("*3\r\n"),
        "MGETT should return *3 array, got: {resp}"
    );
    assert!(
        resp.contains("data1"),
        "MGETT value should contain data1, got: {resp}"
    );
    assert!(
        resp.contains("episodic"),
        "MGETT type should contain episodic, got: {resp}"
    );

    // MGETT on nonexistent key returns null array *-1\r\n
    let resp = send_and_read(&mut stream, "MGETT", &["nonexistent"]).await;
    assert_eq!(resp, "*-1\r\n");

    // MSETT with PX ttl
    let resp = send_and_read(
        &mut stream,
        "MSETT",
        &["mem2", "data2", "semantic", "PX", "60000"],
    )
    .await;
    assert_eq!(resp, "+OK\r\n");

    let resp = send_and_read(&mut stream, "MGETT", &["mem2"]).await;
    assert!(
        resp.starts_with("*3\r\n"),
        "MGETT should return *3 array, got: {resp}"
    );
    assert!(
        resp.contains("data2"),
        "MGETT value should contain data2, got: {resp}"
    );
    assert!(
        resp.contains("semantic"),
        "MGETT type should contain semantic, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// m. REPLICAOF host port is rejected in io_uring mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_uring_replicaof_rejected() {
    let port = start_default_uring_server();
    let mut stream = connect(port).await;

    // REPLICAOF host port should return an error about not supported
    let resp = send_and_read(&mut stream, "REPLICAOF", &["127.0.0.1", "6380"]).await;
    assert!(
        resp.starts_with("-ERR"),
        "REPLICAOF host port should return error in io_uring mode, got: {resp}"
    );
    assert!(
        resp.contains("not supported in io_uring mode"),
        "Error should mention io_uring mode, got: {resp}"
    );
}
