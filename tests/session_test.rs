//! Unit tests for ClientSession (src/resp/session.rs).
//!
//! Covers auth flow, namespace scoping, REPLICAOF actions,
//! pipeline batching, and invalid command format handling.

use std::sync::Arc;

use basalt::http::auth::AuthStore;
use basalt::resp::commands::{CommandHandler, ShareHandler};
use basalt::resp::parser::{RespValue, parse_pipeline};
use basalt::resp::session::{ClientSession, SessionAction};
use basalt::store::share::ShareStore;
use basalt::store::{ConsolidationManager, KvEngine};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a RESP array value representing a single command with arguments.
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

/// Create a CommandHandler backed by a fresh KvEngine with 4 shards.
fn make_handler() -> CommandHandler {
    let engine = Arc::new(KvEngine::new(4, Arc::new(ConsolidationManager::disabled())));
    CommandHandler::new(engine, None)
}

/// Create a ShareHandler backed by fresh ShareStore and AuthStore.
fn make_share_handler() -> ShareHandler {
    let auth = Arc::new(AuthStore::new());
    let share = Arc::new(ShareStore::new());
    ShareHandler::new(share, auth)
}

/// Create a fresh ShareStore wrapped in Arc.
fn make_share() -> Arc<ShareStore> {
    Arc::new(ShareStore::new())
}

/// Human-readable name for a SessionAction variant (no Debug derive).
fn action_name(action: &SessionAction) -> &'static str {
    match action {
        SessionAction::ReplicaofNoOne => "ReplicaofNoOne",
        SessionAction::ReplicaofReplicate { .. } => "ReplicaofReplicate",
        SessionAction::None => "None",
    }
}

// ---------------------------------------------------------------------------
// 1. No-auth PING
// ---------------------------------------------------------------------------

#[test]
fn test_no_auth_ping() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let cmd = make_command("PING", &[]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);

    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("PONG".to_string())
    );
    assert!(matches!(result.action, SessionAction::None));
}

// ---------------------------------------------------------------------------
// 2. No-auth SET + GET
// ---------------------------------------------------------------------------

#[test]
fn test_no_auth_set_get() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let set_cmd = make_command("SET", &["mykey", "myvalue"]);
    let result = session.process_command_batch(&[set_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );

    let get_cmd = make_command("GET", &["mykey"]);
    let result = session.process_command_batch(&[get_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::BulkString(Some(b"myvalue".to_vec()))
    );
}

// ---------------------------------------------------------------------------
// 3. Auth required, unauthenticated -> NOAUTH error
// ---------------------------------------------------------------------------

#[test]
fn test_auth_required_rejects_commands() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "valid-token".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    assert!(!session.is_authenticated());

    let cmd = make_command("PING", &[]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::Error("NOAUTH Authentication required".to_string())
    );
}

// ---------------------------------------------------------------------------
// 4. Auth with valid token, then PING works
// ---------------------------------------------------------------------------

#[test]
fn test_auth_succeeds_then_commands_work() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "valid-token".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // AUTH first
    let auth_cmd = make_command("AUTH", &["valid-token"]);
    let result = session.process_command_batch(&[auth_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );
    assert!(session.is_authenticated());

    // Now PING should work
    let ping_cmd = make_command("PING", &[]);
    let result = session.process_command_batch(&[ping_cmd], &handler, &share_handler);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("PONG".to_string())
    );
}

// ---------------------------------------------------------------------------
// 5. Auth with wrong token
// ---------------------------------------------------------------------------

#[test]
fn test_auth_wrong_token() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "valid-token".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let auth_cmd = make_command("AUTH", &["wrong-token"]);
    let result = session.process_command_batch(&[auth_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::Error("ERR invalid token".to_string())
    );

    // Session should still be unauthenticated
    assert!(!session.is_authenticated());
}

// ---------------------------------------------------------------------------
// 6. Scoped token can access own namespace
// ---------------------------------------------------------------------------

#[test]
fn test_scoped_token_can_access_own_namespace() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // Authenticate with scoped token
    let auth_cmd = make_command("AUTH", &["scoped"]);
    let result = session.process_command_batch(&[auth_cmd], &handler, &share_handler);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );

    // SET in own namespace should succeed
    let set_cmd = make_command("SET", &["ns-alpha:key", "value1"]);
    let result = session.process_command_batch(&[set_cmd], &handler, &share_handler);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );

    // GET from own namespace should succeed
    let get_cmd = make_command("GET", &["ns-alpha:key"]);
    let result = session.process_command_batch(&[get_cmd], &handler, &share_handler);
    assert_eq!(
        result.responses[0],
        RespValue::BulkString(Some(b"value1".to_vec()))
    );
}

// ---------------------------------------------------------------------------
// 7. Scoped token cannot access other namespace
// ---------------------------------------------------------------------------

#[test]
fn test_scoped_token_cannot_access_other_namespace() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // Authenticate with scoped token
    let auth_cmd = make_command("AUTH", &["scoped"]);
    let _ = session.process_command_batch(&[auth_cmd], &handler, &share_handler);

    // SET in a different namespace should be rejected
    let set_cmd = make_command("SET", &["ns-beta:key", "value2"]);
    let result = session.process_command_batch(&[set_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(
                msg.contains("NOAUTH") && msg.contains("ns-beta"),
                "expected NOAUTH error for ns-beta, got: {msg}"
            );
        }
        other => panic!("expected error response, got: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 8. REPLICAOF NO ONE
// ---------------------------------------------------------------------------

#[test]
fn test_replicaof_no_one() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let cmd = make_command("REPLICAOF", &["NO", "ONE"]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);

    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );
    assert!(matches!(result.action, SessionAction::ReplicaofNoOne));
}

// ---------------------------------------------------------------------------
// 9. REPLICAOF host port
// ---------------------------------------------------------------------------

#[test]
fn test_replicaof_replicate() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let cmd = make_command("REPLICAOF", &["127.0.0.1", "6379"]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);

    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );
    match result.action {
        SessionAction::ReplicaofReplicate { host, port } => {
            assert_eq!(host, "127.0.0.1");
            assert_eq!(port, 6379);
        }
        other => panic!("expected ReplicaofReplicate, got {}", action_name(&other)),
    }
}

// ---------------------------------------------------------------------------
// 10. REPLICAOF invalid syntax
// ---------------------------------------------------------------------------

#[test]
fn test_replicaof_invalid() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // No arguments at all
    let cmd = make_command("REPLICAOF", &[]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);

    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(msg.contains("ERR"), "expected ERR in response, got: {msg}");
        }
        other => panic!("expected error response, got: {:?}", other),
    }
    assert!(matches!(result.action, SessionAction::None));
}

// ---------------------------------------------------------------------------
// 11. Pipeline: multiple commands in one batch
// ---------------------------------------------------------------------------

#[test]
fn test_pipeline_multiple_commands() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let ping = make_command("PING", &[]);
    let set_cmd = make_command("SET", &["pipekey", "pipeval"]);
    let get_cmd = make_command("GET", &["pipekey"]);

    let result = session.process_command_batch(&[ping, set_cmd, get_cmd], &handler, &share_handler);

    assert_eq!(result.responses.len(), 3);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("PONG".to_string())
    );
    assert_eq!(
        result.responses[1],
        RespValue::SimpleString("OK".to_string())
    );
    assert_eq!(
        result.responses[2],
        RespValue::BulkString(Some(b"pipeval".to_vec()))
    );
    assert!(matches!(result.action, SessionAction::None));
}

// ---------------------------------------------------------------------------
// 12. Invalid command format (SimpleString instead of Array)
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_command_format() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::new(auth, make_share(), false);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // A SimpleString is not a valid RESP command array
    let bad_value = RespValue::SimpleString("JUST A STRING".to_string());
    let result = session.process_command_batch(&[bad_value], &handler, &share_handler);

    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(
                msg.contains("invalid command format"),
                "expected 'invalid command format' error, got: {msg}"
            );
        }
        other => panic!("expected error response, got: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 13. Key without namespace prefix is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_key_without_namespace_prefix_rejected() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // Authenticate with scoped token
    let auth_cmd = make_command("AUTH", &["scoped"]);
    let _ = session.process_command_batch(&[auth_cmd], &handler, &share_handler);

    // GET with a bare key (no colon) should be rejected with namespace prefix error
    let get_cmd = make_command("GET", &["mykey"]);
    let result = session.process_command_batch(&[get_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(
                msg.contains("namespace prefix"),
                "expected namespace prefix error, got: {msg}"
            );
        }
        other => panic!("expected error response, got: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 14. SET key without namespace prefix is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_set_key_without_namespace_prefix_rejected() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let auth_cmd = make_command("AUTH", &["scoped"]);
    let _ = session.process_command_batch(&[auth_cmd], &handler, &share_handler);

    // SET with a bare key (no colon) should be rejected
    let set_cmd = make_command("SET", &["barekey", "value"]);
    let result = session.process_command_batch(&[set_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(
                msg.contains("namespace prefix"),
                "expected namespace prefix error, got: {msg}"
            );
        }
        other => panic!("expected error response, got: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 15. MGET with mix of namespaced and bare keys is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_mget_bare_key_rejected() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "scoped".to_string(),
        vec!["ns-alpha".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let auth_cmd = make_command("AUTH", &["scoped"]);
    let _ = session.process_command_batch(&[auth_cmd], &handler, &share_handler);

    // MGET where one key lacks namespace prefix
    let mget_cmd = make_command("MGET", &["ns-alpha:key1", "barekey"]);
    let result = session.process_command_batch(&[mget_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    match &result.responses[0] {
        RespValue::Error(msg) => {
            assert!(
                msg.contains("namespace prefix"),
                "expected namespace prefix error, got: {msg}"
            );
        }
        other => panic!("expected error response, got: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 16. Wildcard token allows bare key (no namespace check needed)
// ---------------------------------------------------------------------------

#[test]
fn test_wildcard_token_bypasses_namespace_check() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "admin".to_string(),
        vec!["*".to_string()],
    )]));
    let mut session = ClientSession::new(auth, make_share(), true);
    let handler = make_handler();
    let share_handler = make_share_handler();

    let auth_cmd = make_command("AUTH", &["admin"]);
    let _ = session.process_command_batch(&[auth_cmd], &handler, &share_handler);

    // With wildcard token, even a bare key should work (no namespace check at all)
    let set_cmd = make_command("SET", &["barekey", "value"]);
    let result = session.process_command_batch(&[set_cmd], &handler, &share_handler);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );
}

// ---------------------------------------------------------------------------
// 17. Rate limiting
// ---------------------------------------------------------------------------

#[test]
fn test_rate_limit_allows_within_limit() {
    let auth = Arc::new(AuthStore::new());
    let mut session = ClientSession::with_rate_limit(auth, make_share(), false, 3, 1000);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // 3 PINGs should all succeed
    for _ in 0..3 {
        let cmd = make_command("PING", &[]);
        let result = session.process_command_batch(&[cmd], &handler, &share_handler);
        assert_eq!(result.responses.len(), 1);
        assert_eq!(
            result.responses[0],
            RespValue::SimpleString("PONG".to_string())
        );
    }

    // 4th PING should be rate limited
    let cmd = make_command("PING", &[]);
    let result = session.process_command_batch(&[cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::Error("ERR rate limit exceeded".to_string())
    );
}

#[test]
fn test_rate_limit_auth_bypasses_rate_limit() {
    let auth = Arc::new(AuthStore::from_list(vec![(
        "admin".to_string(),
        vec!["*".to_string()],
    )]));
    // 1 request per window
    let mut session = ClientSession::with_rate_limit(auth.clone(), make_share(), true, 1, 1000);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // AUTH should succeed even when rate limited (AUTH bypasses rate limiting)
    let auth_cmd = make_command("AUTH", &["admin"]);
    let result = session.process_command_batch(&[auth_cmd], &handler, &share_handler);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(
        result.responses[0],
        RespValue::SimpleString("OK".to_string())
    );
}

#[test]
fn test_rate_limit_disabled_when_zero() {
    let auth = Arc::new(AuthStore::new());
    // 0 = disabled
    let mut session = ClientSession::with_rate_limit(auth, make_share(), false, 0, 1000);
    let handler = make_handler();
    let share_handler = make_share_handler();

    // Should allow unlimited requests
    for _ in 0..100 {
        let cmd = make_command("PING", &[]);
        let result = session.process_command_batch(&[cmd], &handler, &share_handler);
        assert_eq!(result.responses.len(), 1);
        assert_eq!(
            result.responses[0],
            RespValue::SimpleString("PONG".to_string())
        );
    }
}
