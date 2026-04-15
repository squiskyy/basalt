use std::sync::Arc;

use crate::http::auth::AuthStore;
use crate::store::share::ShareStore;

use super::commands::{CommandHandler, ReplicaofResult, ShareHandler, check_command_namespace};
use super::parser::{Command, RespValue};

/// Per-connection token bucket for RESP rate limiting.
struct ConnectionBucket {
    /// Number of requests made in the current window.
    count: u64,
    /// Start time of the current window (millisecond timestamp).
    window_start_ms: u64,
}

/// Per-connection session state shared between both server implementations.
pub struct ClientSession {
    pub authenticated: bool,
    pub auth_token: Option<String>,
    pub auth_enabled: bool,
    pub auth: Arc<AuthStore>,
    pub share: Arc<ShareStore>,
    pub is_replica: bool,
    /// Per-connection rate limiter state. None if rate limiting is disabled.
    rate_bucket: Option<ConnectionBucket>,
    /// Max requests per rate limit window (0 = disabled).
    rate_limit_requests: u64,
    /// Rate limit window duration in milliseconds.
    rate_limit_window_ms: u64,
}

/// Special action returned by `process_command_batch` that the caller
/// (server loop) must handle outside the normal response path.
pub enum SessionAction {
    /// Client sent REPLICAOF NO ONE
    ReplicaofNoOne,
    /// Client sent REPLICAOF host port
    ReplicaofReplicate { host: String, port: u16 },
    /// No special action
    None,
}

/// Result of processing a batch of RESP values.
pub struct BatchResult {
    pub responses: Vec<RespValue>,
    pub action: SessionAction,
}

impl ClientSession {
    /// Create a new session. If auth is not required, the session starts
    /// pre-authenticated. If rate_limit_requests is 0, rate limiting is disabled.
    pub fn new(auth: Arc<AuthStore>, share: Arc<ShareStore>, auth_enabled: bool) -> Self {
        Self::with_rate_limit(auth, share, auth_enabled, 0, 1000)
    }

    /// Create a new session with rate limiting configuration.
    pub fn with_rate_limit(
        auth: Arc<AuthStore>,
        share: Arc<ShareStore>,
        auth_enabled: bool,
        rate_limit_requests: u64,
        rate_limit_window_ms: u64,
    ) -> Self {
        let rate_bucket = if rate_limit_requests > 0 {
            Some(ConnectionBucket {
                count: 0,
                window_start_ms: crate::time::now_ms(),
            })
        } else {
            None
        };
        Self {
            authenticated: !auth_enabled,
            auth_token: None,
            auth_enabled,
            auth,
            share,
            is_replica: false,
            rate_bucket,
            rate_limit_requests,
            rate_limit_window_ms: if rate_limit_window_ms == 0 {
                1000
            } else {
                rate_limit_window_ms
            },
        }
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    pub fn is_replica(&self) -> bool {
        self.is_replica
    }

    pub fn set_replica(&mut self, value: bool) {
        self.is_replica = value;
    }

    /// Check if this connection is rate limited. Returns true if the request
    /// is allowed, false if rate limited.
    fn check_rate_limit(&mut self) -> bool {
        if let Some(ref mut bucket) = self.rate_bucket {
            let now_ms = crate::time::now_ms();
            // Reset window if expired
            if now_ms - bucket.window_start_ms >= self.rate_limit_window_ms {
                bucket.count = 0;
                bucket.window_start_ms = now_ms;
            }
            if bucket.count >= self.rate_limit_requests {
                return false;
            }
            bucket.count += 1;
        }
        true
    }

    /// Process a batch of parsed RESP values and return responses plus
    /// any special session action (e.g. REPLICAOF).
    pub fn process_command_batch(
        &mut self,
        values: &[RespValue],
        handler: &CommandHandler,
        share_handler: &ShareHandler,
    ) -> BatchResult {
        let mut responses: Vec<RespValue> = Vec::with_capacity(values.len());
        let mut action = SessionAction::None;

        for value in values {
            // Check for AUTH and REPLICAOF commands first (always allowed)
            if let Some(cmd) = value.to_command() {
                if cmd.name == "AUTH" {
                    let resp = handle_auth(
                        &cmd,
                        &self.auth,
                        &mut self.authenticated,
                        &mut self.auth_token,
                    );
                    responses.push(resp);
                    continue;
                }

                if cmd.name == "REPLICAOF" {
                    let result = handler.handle_replicaof(&cmd);
                    match result {
                        ReplicaofResult::NoOne => {
                            action = SessionAction::ReplicaofNoOne;
                            responses.push(RespValue::SimpleString("OK".to_string()));
                        }
                        ReplicaofResult::Replicate { host, port } => {
                            action = SessionAction::ReplicaofReplicate {
                                host: host.clone(),
                                port,
                            };
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

            // Per-connection rate limiting check
            if !self.check_rate_limit() {
                responses.push(RespValue::Error("ERR rate limit exceeded".to_string()));
                continue;
            }

            match value.to_command() {
                Some(cmd) => {
                    let cmd_name = cmd.name.to_uppercase();

                    // Share commands are handled by ShareHandler
                    if cmd_name == "SHARE"
                        || cmd_name == "SHAREDEL"
                        || cmd_name == "SHARELIST"
                        || cmd_name == "SHAREWITH"
                    {
                        if let Some(ref token) = self.auth_token {
                            let resp = share_handler.handle(&cmd, token);
                            responses.push(resp);
                        } else {
                            responses.push(RespValue::Error(
                                "NOAUTH Authentication required".to_string(),
                            ));
                        }
                        continue;
                    }

                    // Per-command namespace authorization check (with sharing support)
                    if let Some(ref token) = self.auth_token
                        && let Err(err_resp) =
                            check_command_namespace(&cmd, &self.auth, &self.share, token)
                    {
                        responses.push(err_resp);
                        continue;
                    }
                    let resp = handler.handle(&cmd);
                    responses.push(resp);
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
