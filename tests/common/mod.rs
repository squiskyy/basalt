//! Shared test helpers for Basalt integration tests.
//!
//! This module is imported via `mod common;` from integration test files
//! under `tests/`. It provides RESP encoding/parsing helpers and
//! convenience constructors for KvEngine and AuthStore.

use std::sync::Arc;

use basalt::store::{ConsolidationManager, KvEngine};
use basalt::http::auth::AuthStore;

// ---------------------------------------------------------------------------
// RESP helpers
// ---------------------------------------------------------------------------

/// Build a RESP-encoded command array: *N\r\n$len\r\nname\r\n...
pub fn encode_resp_command(name: &str, args: &[&str]) -> Vec<u8> {
    let total_args = 1 + args.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("*{total_args}\r\n").as_bytes());
    // Command name
    buf.extend_from_slice(format!("${}\r\n{name}\r\n", name.len()).as_bytes());
    // Arguments
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
        b'+' | b'-' => {
            // Simple string or error: find \r\n
            find_crlf(buf).map(|pos| pos + 2)
        }
        b':' => {
            // Integer: find \r\n
            find_crlf(buf).map(|pos| pos + 2)
        }
        b'$' => {
            // Bulk string: find first \r\n for length, then data + \r\n
            let crlf_pos = find_crlf(buf)?;
            let len_end = crlf_pos + 2; // after the \r\n of the length line
            let len_str = std::str::from_utf8(&buf[1..crlf_pos]).ok()?;
            let len: i64 = len_str.parse().ok()?;
            if len == -1 {
                return Some(len_end);
            }
            let data_end = len_end + (len as usize) + 2; // data + \r\n
            if buf.len() >= data_end {
                Some(data_end)
            } else {
                None
            }
        }
        b'*' => {
            // Array: find first \r\n for count, then recursively parse elements
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

// ---------------------------------------------------------------------------
// Engine / Auth helpers
// ---------------------------------------------------------------------------

/// Create a new KvEngine with 4 shards.
pub fn make_engine() -> Arc<KvEngine> {
    Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())))
}

/// Create an AuthStore with no tokens (auth disabled).
pub fn make_no_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::new())
}

/// Create an AuthStore with a wildcard token that can access all namespaces.
pub fn make_wildcard_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::from_list(vec![(
        "bsk-test-token".to_string(),
        vec!["*".to_string()],
    )]))
}

/// Create an AuthStore with scoped and wildcard tokens.
pub fn make_scoped_auth() -> Arc<AuthStore> {
    Arc::new(AuthStore::from_list(vec![
        ("bsk-scoped".to_string(), vec!["ns-alpha".to_string()]),
        ("bsk-wildcard".to_string(), vec!["*".to_string()]),
    ]))
}
