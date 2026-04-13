//! RESP integration tests for Basalt.
//!
//! These tests start the RESP server on a random port, connect via
//! tokio::net::TcpStream, and exercise the full command set by writing
//! RESP-encoded commands and reading/parsing the responses.

mod common;

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::resp::server::run as resp_run;
use basalt::store::engine::KvEngine;
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

/// Start the RESP server on a random port. Returns (port, JoinHandle).
async fn start_resp_server(
    engine: Arc<KvEngine>,
    auth: Arc<AuthStore>,
    share: Arc<ShareStore>,
    db_path: Option<String>,
) -> (u16, tokio::task::JoinHandle<()>) {
    // We need to find the actual port. Bind a listener ourselves, get the
    // port, then pass it to the server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        // We can't easily reuse the listener from `run`, so we'll start the
        // server by calling `run` with the port. But `run` binds its own
        // listener. So we drop ours and let `run` bind the same port.
        drop(listener);
        // Small delay to ensure the port is released
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // If run fails (e.g., port conflict), that's a test failure
        let _ = resp_run("127.0.0.1", port, engine, auth, share, db_path).await;
    });

    // Give the server time to bind
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    (port, handle)
}

/// Convenience: start a RESP server with no auth, returning (port, handle).
async fn start_default_resp_server() -> (u16, tokio::task::JoinHandle<()>) {
    start_resp_server(
        make_engine(),
        make_no_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
    .await
}

/// Start a RESP server with auth enabled, returning (port, handle).
async fn start_auth_resp_server() -> (u16, tokio::task::JoinHandle<()>) {
    start_resp_server(
        make_engine(),
        make_wildcard_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
    .await
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
async fn test_ping_returns_pong() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "PING", &[]).await;
    assert_eq!(resp, "+PONG\r\n");
}

// ---------------------------------------------------------------------------
// b. SET key value returns +OK
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_set_returns_ok() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "SET", &["mykey", "myval"]).await;
    assert_eq!(resp, "+OK\r\n");
}

// ---------------------------------------------------------------------------
// c. GET key returns bulk string value
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_returns_bulk_string() {
    let (port, _handle) = start_default_resp_server().await;
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
async fn test_get_nonexistent_returns_null() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "GET", &["nonexistent"]).await;
    assert_eq!(resp, "$-1\r\n");
}

// ---------------------------------------------------------------------------
// e. DEL key returns :1, DEL nonexistent returns :0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_del_returns_count() {
    let (port, _handle) = start_default_resp_server().await;
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
async fn test_mset_mget() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    // MSET
    let resp = send_and_read(&mut stream, "MSET", &["k1", "v1", "k2", "v2"]).await;
    assert_eq!(resp, "+OK\r\n");

    // MGET should return an array of two bulk strings
    let resp = send_and_read(&mut stream, "MGET", &["k1", "k2"]).await;
    // *2\r\n$2\r\nv1\r\n$2\r\nv2\r\n
    assert_eq!(resp, "*2\r\n$2\r\nv1\r\n$2\r\nv2\r\n");
}

// ---------------------------------------------------------------------------
// g. INFO returns a bulk string with server info
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_info_returns_bulk_string() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    let resp = send_and_read(&mut stream, "INFO", &[]).await;
    // Should start with $ (bulk string) and contain "basalt_version"
    assert!(
        resp.starts_with('$'),
        "INFO response should be a bulk string, got: {resp}"
    );
    // Decode the bulk string to check content
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
async fn test_auth_flow() {
    let (port, _handle) = start_auth_resp_server().await;
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
async fn test_auth_wrong_token() {
    let (port, _handle) = start_auth_resp_server().await;
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

/// Start a RESP server with a scoped (non-wildcard) auth token.
async fn start_scoped_auth_resp_server() -> (u16, tokio::task::JoinHandle<()>) {
    start_resp_server(
        make_engine(),
        make_scoped_auth(),
        Arc::new(ShareStore::new()),
        None,
    )
    .await
}

#[tokio::test]
async fn test_auth_scoped_token_can_access_own_namespace() {
    let (port, _handle) = start_scoped_auth_resp_server().await;
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
async fn test_auth_scoped_token_cannot_access_other_namespace() {
    let (port, _handle) = start_scoped_auth_resp_server().await;
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
async fn test_auth_wildcard_token_can_access_any_namespace() {
    let (port, _handle) = start_scoped_auth_resp_server().await;
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
async fn test_pipelining() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    // Pipeline: PING, SET foo bar, GET foo
    let mut pipeline = Vec::new();
    pipeline.extend(encode_resp_command("PING", &[]));
    pipeline.extend(encode_resp_command("SET", &["foo", "bar"]));
    pipeline.extend(encode_resp_command("GET", &["foo"]));

    stream.write_all(&pipeline).await.unwrap();
    stream.flush().await.unwrap();

    // Read all responses — they should come back as a batch
    // We need to read enough bytes to get all 3 responses
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
        // If we haven't parsed 3 yet, read more
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

    // Verify exact ordering: +PONG\r\n+OK\r\n$3\r\nbar\r\n
    assert_eq!(response, "+PONG\r\n+OK\r\n$3\r\nbar\r\n");
}

// ---------------------------------------------------------------------------
// k. MSETT and MGETT commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_msett_mgett() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    // MSETT key value type [PX ms]
    let resp = send_and_read(&mut stream, "MSETT", &["mem1", "data1", "episodic"]).await;
    assert_eq!(resp, "+OK\r\n");

    // MGETT key → returns array [value, type, ttl]
    let resp = send_and_read(&mut stream, "MGETT", &["mem1"]).await;
    // Should be a 3-element array: bulk strings for value, type, and ttl
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
// l. MSCAN prefix command
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mscan() {
    let (port, _handle) = start_default_resp_server().await;
    let mut stream = connect(port).await;

    // Insert some keys with a common prefix
    let _ = send_and_read(&mut stream, "SET", &["user:1", "alice"]).await;
    let _ = send_and_read(&mut stream, "SET", &["user:2", "bob"]).await;
    let _ = send_and_read(&mut stream, "SET", &["other:x", "charlie"]).await;

    // MSCAN with prefix "user:"
    let resp = send_and_read(&mut stream, "MSCAN", &["user:"]).await;
    // Should return an array of entries, each being [key, value, type, ttl]
    // Since SET uses Semantic type with no TTL
    assert!(
        resp.starts_with('*'),
        "MSCAN should return an array, got: {resp}"
    );

    // The array should have 2 entries (user:1 and user:2)
    // Parse the array count
    let crlf_pos = resp.find("\r\n").unwrap();
    let count_str = &resp[1..crlf_pos];
    let count: usize = count_str.parse().unwrap();
    assert_eq!(
        count, 2,
        "MSCAN should return 2 entries for prefix 'user:', got: {resp}"
    );

    // Verify the response contains our data
    assert!(
        resp.contains("user:1"),
        "MSCAN should contain user:1, got: {resp}"
    );
    assert!(
        resp.contains("user:2"),
        "MSCAN should contain user:2, got: {resp}"
    );
    assert!(
        resp.contains("alice"),
        "MSCAN should contain alice, got: {resp}"
    );
    assert!(
        resp.contains("bob"),
        "MSCAN should contain bob, got: {resp}"
    );
    assert!(
        !resp.contains("charlie"),
        "MSCAN should NOT contain charlie, got: {resp}"
    );

    // MSCAN with a prefix that matches nothing returns empty array
    let resp = send_and_read(&mut stream, "MSCAN", &["nonexistent:"]).await;
    assert_eq!(
        resp, "*0\r\n",
        "MSCAN with no matches should return *0\\r\\n, got: {resp}"
    );
}
