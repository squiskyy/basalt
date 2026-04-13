use std::sync::Arc;

use crate::http::auth::AuthStore;
use crate::store::share::ShareStore;

use super::commands::{CommandHandler, ReplicaofResult, ShareHandler, check_command_namespace};
use super::parser::{Command, RespValue};

/// Per-connection session state shared between both server implementations.
pub struct ClientSession {
    pub authenticated: bool,
    pub auth_token: Option<String>,
    pub auth_enabled: bool,
    pub auth: Arc<AuthStore>,
    pub share: Arc<ShareStore>,
    pub is_replica: bool,
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
    /// pre-authenticated.
    pub fn new(auth: Arc<AuthStore>, share: Arc<ShareStore>, auth_enabled: bool) -> Self {
        Self {
            authenticated: !auth_enabled,
            auth_token: None,
            auth_enabled,
            auth,
            share,
            is_replica: false,
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
