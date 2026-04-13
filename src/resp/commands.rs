use std::sync::Arc;

use crate::replication::ReplicationState;
use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;

use super::parser::{Command, RespValue};

/// Extract the namespace from a key in "namespace:key" format.
/// Returns the namespace portion (before the first ':'), or None if no ':' is present.
/// For constructing namespaced keys, prefer `NamespacedKey::new()`.
pub fn extract_namespace(key: &str) -> Option<&str> {
    key.find(':').map(|pos| &key[..pos])
}

/// Commands that do not access any key and thus need no per-command namespace check.
const NO_KEY_COMMANDS: &[&str] = &["PING", "INFO", "SNAP", "AUTH", "REPLICAOF"];

/// Return the first key-carrying argument index for a given command name.
/// Returns None for commands that carry no keys.
fn first_key_arg_index(cmd_name: &str) -> Option<usize> {
    match cmd_name.to_uppercase().as_str() {
        "GET" | "DEL" | "MGETT" | "MTYPE" => Some(0),
        "SET" | "MSETT" => Some(0),
        "MGET" => Some(0),           // multiple keys starting at arg 0
        "MSET" => Some(0),           // key-value pairs starting at arg 0
        "KEYS" | "MSCAN" => Some(0), // prefix/pattern arg
        "VSEARCH" => Some(0),        // namespace arg
        _ => None,
    }
}

/// Check whether a command is authorized for a given auth token by extracting
/// the namespace from its key arguments and verifying against the AuthStore.
/// Returns Ok(()) if authorized, or Err(response) with a RESP error.
pub fn check_command_namespace(
    cmd: &Command,
    auth: &crate::http::auth::AuthStore,
    token: &str,
) -> Result<(), RespValue> {
    let name = cmd.name.to_uppercase();

    // Commands that don't touch keys don't need namespace checks
    if NO_KEY_COMMANDS.contains(&name.as_str()) {
        return Ok(());
    }

    // If token has wildcard access, skip per-key checks
    if auth.is_authorized(token, "*") {
        return Ok(());
    }

    let key_index = match first_key_arg_index(&name) {
        Some(i) => i,
        None => return Ok(()), // unknown command, let it through to CommandHandler
    };

    if cmd.args.is_empty() {
        return Ok(()); // will be caught by command handler arg validation
    }

    // For MSET, keys are at even indices (0, 2, 4, ...); check all of them
    if name == "MSET" {
        let mut i = 0;
        while i < cmd.args.len() {
            let key = String::from_utf8_lossy(&cmd.args[i]).to_string();
            let ns = match extract_namespace(&key) {
                Some(ns) => ns,
                None => {
                    return Err(RespValue::Error(
                        "ERR key must include namespace prefix (format: namespace:key)".to_string(),
                    ));
                }
            };
            if !auth.is_authorized(token, ns) {
                return Err(RespValue::Error(format!(
                    "NOAUTH Token not authorized for namespace '{}'",
                    ns
                )));
            }
            i += 2;
        }
        return Ok(());
    }

    // For MGET, all args are keys
    if name == "MGET" || name == "DEL" {
        for arg in &cmd.args {
            let key = String::from_utf8_lossy(arg).to_string();
            let ns = match extract_namespace(&key) {
                Some(ns) => ns,
                None => {
                    return Err(RespValue::Error(
                        "ERR key must include namespace prefix (format: namespace:key)".to_string(),
                    ));
                }
            };
            if !auth.is_authorized(token, ns) {
                return Err(RespValue::Error(format!(
                    "NOAUTH Token not authorized for namespace '{}'",
                    ns
                )));
            }
        }
        return Ok(());
    }

    // For single-key commands, check the first key arg
    if key_index < cmd.args.len() {
        let key = String::from_utf8_lossy(&cmd.args[key_index]).to_string();
        let ns = match extract_namespace(&key) {
            Some(ns) => ns,
            None => {
                return Err(RespValue::Error(
                    "ERR key must include namespace prefix (format: namespace:key)".to_string(),
                ));
            }
        };
        if !auth.is_authorized(token, ns) {
            return Err(RespValue::Error(format!(
                "NOAUTH Token not authorized for namespace '{}'",
                ns
            )));
        }
    }

    Ok(())
}

/// Handles RESP commands by dispatching to the KvEngine.
pub struct CommandHandler {
    engine: Arc<KvEngine>,
    db_path: Option<String>,
    repl_state: Option<Arc<ReplicationState>>,
}

impl CommandHandler {
    pub fn new(engine: Arc<KvEngine>, db_path: Option<String>) -> Self {
        CommandHandler {
            engine,
            db_path,
            repl_state: None,
        }
    }

    pub fn with_replication(
        engine: Arc<KvEngine>,
        db_path: Option<String>,
        repl_state: Arc<ReplicationState>,
    ) -> Self {
        CommandHandler {
            engine,
            db_path,
            repl_state: Some(repl_state),
        }
    }

    /// Dispatch a command and return a RESP value response.
    pub fn handle(&self, cmd: &Command) -> RespValue {
        let name = cmd.name.to_uppercase();
        match name.as_str() {
            "PING" => self.handle_ping(cmd),
            "SET" => self.handle_set(cmd),
            "GET" => self.handle_get(cmd),
            "DEL" => self.handle_del(cmd),
            "MGET" => self.handle_mget(cmd),
            "MSET" => self.handle_mset(cmd),
            "KEYS" => self.handle_keys(cmd),
            "INFO" => self.handle_info(cmd),
            // Basalt-specific commands
            "MSETT" => self.handle_msett(cmd),
            "MGETT" => self.handle_mgett(cmd),
            "MSCAN" => self.handle_mscan(cmd),
            "MTYPE" => self.handle_mtype(cmd),
            "SNAP" => self.handle_snap(cmd),
            "VSEARCH" => self.handle_vsearch(cmd),
            "REPLICAOF" => {
                RespValue::Error("ERR REPLICAOF must be handled at connection level".to_string())
            }
            // AUTH is handled separately in the connection handler
            "AUTH" => RespValue::Error("ERR AUTH already handled at connection level".to_string()),
            _ => RespValue::Error(format!("ERR unknown command '{}'", cmd.name)),
        }
    }

    /// Handle REPLICAOF command — returns a special result that the connection handler
    /// uses to initiate replication. This is not a normal RESP response.
    pub fn handle_replicaof(&self, cmd: &Command) -> ReplicaofResult {
        if cmd.args.is_empty() {
            return ReplicaofResult::Error(
                "ERR wrong number of arguments for 'REPLICAOF'".to_string(),
            );
        }
        let first = String::from_utf8_lossy(&cmd.args[0]).to_uppercase();
        if first == "NO" && cmd.args.len() == 2 {
            let second = String::from_utf8_lossy(&cmd.args[1]).to_uppercase();
            if second == "ONE" {
                return ReplicaofResult::NoOne;
            }
        }
        if cmd.args.len() == 2 {
            let host = String::from_utf8_lossy(&cmd.args[0]).to_string();
            let port_str = String::from_utf8_lossy(&cmd.args[1]);
            match port_str.parse::<u16>() {
                Ok(port) => return ReplicaofResult::Replicate { host, port },
                Err(_) => {
                    return ReplicaofResult::Error("ERR invalid port for REPLICAOF".to_string());
                }
            }
        }
        ReplicaofResult::Error(
            "ERR syntax error for REPLICAOF. Use: REPLICAOF host port | REPLICAOF NO ONE"
                .to_string(),
        )
    }

    fn handle_ping(&self, cmd: &Command) -> RespValue {
        if cmd.args.is_empty() {
            RespValue::SimpleString("PONG".to_string())
        } else {
            // PING with message echoes it back as a BulkString per Redis convention
            let msg = String::from_utf8_lossy(&cmd.args[0]).to_string();
            RespValue::BulkString(Some(msg.into_bytes()))
        }
    }

    fn handle_set(&self, cmd: &Command) -> RespValue {
        // SET key value [EX seconds] [PX milliseconds] [NX] [XX]
        if cmd.args.len() < 2 {
            return RespValue::Error("ERR wrong number of arguments for 'SET'".to_string());
        }

        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        let value = cmd.args[1].clone();
        let mut ttl_ms: Option<u64> = None;
        let mut nx = false;
        let mut xx = false;

        let mut i = 2;
        while i < cmd.args.len() {
            let flag = String::from_utf8_lossy(&cmd.args[i]).to_uppercase();
            match flag.as_str() {
                "EX" => {
                    if i + 1 >= cmd.args.len() {
                        return RespValue::Error(
                            "ERR syntax error — EX requires a value".to_string(),
                        );
                    }
                    let secs: u64 = match String::from_utf8_lossy(&cmd.args[i + 1]).parse() {
                        Ok(s) => s,
                        Err(_) => {
                            return RespValue::Error(
                                "ERR value is not an integer or out of range".to_string(),
                            );
                        }
                    };
                    ttl_ms = Some(secs * 1000);
                    i += 2;
                }
                "PX" => {
                    if i + 1 >= cmd.args.len() {
                        return RespValue::Error(
                            "ERR syntax error — PX requires a value".to_string(),
                        );
                    }
                    let ms: u64 = match String::from_utf8_lossy(&cmd.args[i + 1]).parse() {
                        Ok(m) => m,
                        Err(_) => {
                            return RespValue::Error(
                                "ERR value is not an integer or out of range".to_string(),
                            );
                        }
                    };
                    ttl_ms = Some(ms);
                    i += 2;
                }
                "NX" => {
                    nx = true;
                    i += 1;
                }
                "XX" => {
                    xx = true;
                    i += 1;
                }
                _ => {
                    return RespValue::Error(format!("ERR syntax error — unknown flag '{flag}'"));
                }
            }
        }

        if nx && xx {
            return RespValue::Error(
                "ERR syntax error — NX and XX flags are mutually exclusive".to_string(),
            );
        }

        // NX: only set if key does not exist
        if nx && self.engine.get(&key).is_some() {
            return RespValue::BulkString(None);
        }

        // XX: only set if key exists
        if xx && self.engine.get(&key).is_none() {
            return RespValue::BulkString(None);
        }

        let mem_type = MemoryType::Semantic;
        match self.engine.set(&key, value.clone(), ttl_ms, mem_type) {
            Ok(()) => {
                if let Some(ref repl) = self.repl_state {
                    repl.record_set(key.as_bytes(), &value, mem_type, ttl_ms);
                }
                RespValue::SimpleString("OK".to_string())
            }
            Err(e) => RespValue::Error(format!(
                "ERR max entries exceeded (shard {}, {}/{})",
                e.shard_index, e.current, e.max_entries
            )),
        }
    }

    fn handle_get(&self, cmd: &Command) -> RespValue {
        if cmd.args.len() != 1 {
            return RespValue::Error("ERR wrong number of arguments for 'GET'".to_string());
        }
        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        match self.engine.get(&key) {
            Some(val) => RespValue::BulkString(Some(val)),
            None => RespValue::BulkString(None),
        }
    }

    fn handle_del(&self, cmd: &Command) -> RespValue {
        if cmd.args.is_empty() {
            return RespValue::Error("ERR wrong number of arguments for 'DEL'".to_string());
        }
        let mut deleted: i64 = 0;
        for arg in &cmd.args {
            let key = String::from_utf8_lossy(arg).to_string();
            if self.engine.delete(&key) {
                if let Some(ref repl) = self.repl_state {
                    repl.record_delete(key.as_bytes());
                }
                deleted += 1;
            }
        }
        RespValue::Integer(deleted)
    }

    fn handle_mget(&self, cmd: &Command) -> RespValue {
        if cmd.args.is_empty() {
            return RespValue::Error("ERR wrong number of arguments for 'MGET'".to_string());
        }
        let results: Vec<RespValue> = cmd
            .args
            .iter()
            .map(|arg| {
                let key = String::from_utf8_lossy(arg).to_string();
                match self.engine.get(&key) {
                    Some(val) => RespValue::BulkString(Some(val)),
                    None => RespValue::BulkString(None),
                }
            })
            .collect();
        RespValue::Array(Some(results))
    }

    fn handle_mset(&self, cmd: &Command) -> RespValue {
        if cmd.args.len() < 2 || !cmd.args.len().is_multiple_of(2) {
            return RespValue::Error("ERR wrong number of arguments for 'MSET'".to_string());
        }
        let mut i = 0;
        while i < cmd.args.len() {
            let key = String::from_utf8_lossy(&cmd.args[i]).to_string();
            let value = cmd.args[i + 1].clone();
            match self
                .engine
                .set(&key, value.clone(), None, MemoryType::Semantic)
            {
                Ok(()) => {
                    if let Some(ref repl) = self.repl_state {
                        repl.record_set(key.as_bytes(), &value, MemoryType::Semantic, None);
                    }
                }
                Err(e) => {
                    return RespValue::Error(format!(
                        "ERR max entries exceeded (shard {}, {}/{})",
                        e.shard_index, e.current, e.max_entries
                    ));
                }
            }
            i += 2;
        }
        RespValue::SimpleString("OK".to_string())
    }

    fn handle_keys(&self, cmd: &Command) -> RespValue {
        // KEYS only supports prefix patterns ending with *
        if cmd.args.len() != 1 {
            return RespValue::Error("ERR wrong number of arguments for 'KEYS'".to_string());
        }
        let pattern = String::from_utf8_lossy(&cmd.args[0]).to_string();
        if !pattern.ends_with('*') {
            return RespValue::Error(
                "ERR KEYS only supports prefix patterns ending with '*'".to_string(),
            );
        }
        let prefix = &pattern[..pattern.len() - 1];
        let entries = self.engine.scan_prefix(prefix);
        let keys: Vec<RespValue> = entries
            .into_iter()
            .map(|(k, _, _)| RespValue::BulkString(Some(k.into_bytes())))
            .collect();
        RespValue::Array(Some(keys))
    }

    /// RESP INFO handler - return server information.
    fn handle_info(&self, cmd: &Command) -> RespValue {
        let section = if !cmd.args.is_empty() {
            String::from_utf8_lossy(&cmd.args[0]).to_lowercase()
        } else {
            String::new()
        };

        if section == "replication" {
            if let Some(ref repl) = self.repl_state {
                return RespValue::BulkString(Some(repl.info_string().into_bytes()));
            } else {
                return RespValue::BulkString(Some(
                    "# Replication\r\nrole:primary\r\nconnected_replicas:0\r\nreplication_offset:0\r\nwal_size:0\r\n".to_string().into_bytes(),
                ));
            }
        }

        // Default INFO response
        let shard_count = self.engine.shard_count();
        let mut shard_entries = Vec::with_capacity(shard_count);
        for i in 0..shard_count {
            shard_entries.push(self.engine.shard_entry_count(i));
        }
        let shard_entries_str = shard_entries
            .iter()
            .enumerate()
            .map(|(i, c)| format!("shard_{i}_entries:{c}"))
            .collect::<Vec<_>>()
            .join("\r\n");

        let info = format!(
            "# Basalt\r\nbasalt_version:0.1.0\r\nshard_count:{shard_count}\r\ncompression_threshold:{}\r\neviction_policy:{}\r\nmax_entries_per_shard:{}\r\n{shard_entries_str}\r\n",
            self.engine.compression_threshold(),
            self.engine.eviction_policy(),
            self.engine.max_entries(),
        );
        RespValue::BulkString(Some(info.into_bytes()))
    }

    // -- Basalt-specific commands --

    fn handle_msett(&self, cmd: &Command) -> RespValue {
        // MSETT key value type [PX ms]
        if cmd.args.len() < 3 {
            return RespValue::Error("ERR wrong number of arguments for 'MSETT'".to_string());
        }

        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        let value = cmd.args[1].clone();
        let type_str = String::from_utf8_lossy(&cmd.args[2]).to_lowercase();
        let memory_type = match type_str.as_str() {
            "episodic" => MemoryType::Episodic,
            "semantic" => MemoryType::Semantic,
            "procedural" => MemoryType::Procedural,
            _ => {
                return RespValue::Error(
                    "ERR type must be 'episodic', 'semantic', or 'procedural'".to_string(),
                );
            }
        };

        let mut ttl_ms: Option<u64> = None;
        let mut i = 3;
        while i < cmd.args.len() {
            let flag = String::from_utf8_lossy(&cmd.args[i]).to_uppercase();
            match flag.as_str() {
                "PX" => {
                    if i + 1 >= cmd.args.len() {
                        return RespValue::Error(
                            "ERR syntax error — PX requires a value".to_string(),
                        );
                    }
                    let ms: u64 = match String::from_utf8_lossy(&cmd.args[i + 1]).parse() {
                        Ok(m) => m,
                        Err(_) => {
                            return RespValue::Error(
                                "ERR value is not an integer or out of range".to_string(),
                            );
                        }
                    };
                    ttl_ms = Some(ms);
                    i += 2;
                }
                _ => {
                    return RespValue::Error(format!("ERR syntax error — unknown flag '{flag}'"));
                }
            }
        }

        match self.engine.set(&key, value.clone(), ttl_ms, memory_type) {
            Ok(()) => {
                if let Some(ref repl) = self.repl_state {
                    repl.record_set(key.as_bytes(), &value, memory_type, ttl_ms);
                }
                RespValue::SimpleString("OK".to_string())
            }
            Err(e) => RespValue::Error(format!(
                "ERR max entries exceeded (shard {}, {}/{})",
                e.shard_index, e.current, e.max_entries
            )),
        }
    }

    fn handle_mgett(&self, cmd: &Command) -> RespValue {
        // MGETT key → returns array [value, type, ttl]
        if cmd.args.len() != 1 {
            return RespValue::Error("ERR wrong number of arguments for 'MGETT'".to_string());
        }
        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        match self.engine.get_with_meta(&key) {
            Some((value, meta)) => {
                let type_str = meta.memory_type.to_string();
                let ttl_str = match meta.ttl_remaining_ms {
                    Some(ms) => ms.to_string(),
                    None => "-1".to_string(),
                };
                RespValue::Array(Some(vec![
                    RespValue::BulkString(Some(value)),
                    RespValue::BulkString(Some(type_str.into_bytes())),
                    RespValue::BulkString(Some(ttl_str.into_bytes())),
                ]))
            }
            None => RespValue::Array(None),
        }
    }

    fn handle_mscan(&self, cmd: &Command) -> RespValue {
        // MSCAN prefix → returns array of [key, value, type, ttl] arrays
        if cmd.args.len() != 1 {
            return RespValue::Error("ERR wrong number of arguments for 'MSCAN'".to_string());
        }
        let prefix = String::from_utf8_lossy(&cmd.args[0]).to_string();
        let entries = self.engine.scan_prefix(&prefix);
        let results: Vec<RespValue> = entries
            .into_iter()
            .map(|(key, value, meta)| {
                let type_str = meta.memory_type.to_string();
                let ttl_str = match meta.ttl_remaining_ms {
                    Some(ms) => ms.to_string(),
                    None => "-1".to_string(),
                };
                RespValue::Array(Some(vec![
                    RespValue::BulkString(Some(key.into_bytes())),
                    RespValue::BulkString(Some(value)),
                    RespValue::BulkString(Some(type_str.into_bytes())),
                    RespValue::BulkString(Some(ttl_str.into_bytes())),
                ]))
            })
            .collect();
        RespValue::Array(Some(results))
    }

    fn handle_mtype(&self, cmd: &Command) -> RespValue {
        // MTYPE key → returns the memory type as a simple string, or "none"
        if cmd.args.len() != 1 {
            return RespValue::Error("ERR wrong number of arguments for 'MTYPE'".to_string());
        }
        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        match self.engine.get_with_meta(&key) {
            Some((_value, meta)) => RespValue::SimpleString(meta.memory_type.to_string()),
            None => RespValue::SimpleString("none".to_string()),
        }
    }

    fn handle_snap(&self, _cmd: &Command) -> RespValue {
        // SNAP — trigger a manual snapshot to disk
        match &self.db_path {
            Some(db_path) => {
                let path = std::path::Path::new(db_path);
                match crate::store::persistence::snapshot(path, &self.engine, 3) {
                    Ok(snapshot_path) => {
                        let entries =
                            crate::store::persistence::collect_entries(&self.engine).len();
                        RespValue::BulkString(Some(
                            format!(
                                "OK snapshot saved: {} ({} entries)",
                                snapshot_path.display(),
                                entries
                            )
                            .into_bytes(),
                        ))
                    }
                    Err(e) => RespValue::Error(format!("ERR snapshot failed: {e}")),
                }
            }
            None => {
                RespValue::Error("ERR no db_path configured; persistence is disabled".to_string())
            }
        }
    }

    fn handle_vsearch(&self, cmd: &Command) -> RespValue {
        // VSEARCH <namespace> <embedding_json> [COUNT <n>]
        if cmd.args.len() < 2 {
            return RespValue::Error("ERR wrong number of arguments for 'VSEARCH'".to_string());
        }

        let namespace = String::from_utf8_lossy(&cmd.args[0]).to_string();

        // Parse embedding JSON array
        let embedding_json = String::from_utf8_lossy(&cmd.args[1]);
        let embedding: Vec<f32> = match serde_json::from_str(&embedding_json) {
            Ok(vec) => vec,
            Err(e) => {
                return RespValue::Error(format!("ERR invalid embedding JSON: {e}"));
            }
        };

        // Parse optional COUNT parameter
        let mut top_k: usize = 10;
        let mut i = 2;
        while i < cmd.args.len() {
            let flag = String::from_utf8_lossy(&cmd.args[i]).to_uppercase();
            match flag.as_str() {
                "COUNT" => {
                    if i + 1 >= cmd.args.len() {
                        return RespValue::Error(
                            "ERR syntax error — COUNT requires a value".to_string(),
                        );
                    }
                    match String::from_utf8_lossy(&cmd.args[i + 1]).parse::<usize>() {
                        Ok(n) => top_k = n,
                        Err(_) => {
                            return RespValue::Error(
                                "ERR COUNT value must be a positive integer".to_string(),
                            );
                        }
                    }
                    i += 2;
                }
                _ => {
                    return RespValue::Error(format!("ERR syntax error — unknown flag '{flag}'"));
                }
            }
        }

        let results = self.engine.search_embedding(&namespace, &embedding, top_k);

        // Return as flat array: [key1, distance1, value1, key2, distance2, value2, ...]
        let items: Vec<RespValue> = results
            .into_iter()
            .flat_map(|r| {
                let dist_str = format!("{:.6}", r.distance);
                vec![
                    RespValue::BulkString(Some(r.key.into_bytes())),
                    RespValue::BulkString(Some(dist_str.into_bytes())),
                    RespValue::BulkString(Some(r.value)),
                ]
            })
            .collect();

        RespValue::Array(Some(items))
    }
}

/// Result of REPLICAOF command, used by the connection handler.
pub enum ReplicaofResult {
    /// REPLICAOF host port
    Replicate { host: String, port: u16 },
    /// REPLICAOF NO ONE
    NoOne,
    /// Error
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler() -> CommandHandler {
        CommandHandler::new(Arc::new(KvEngine::new(4)), None)
    }

    #[test]
    fn test_ping() {
        let handler = make_handler();
        let cmd = Command {
            name: "PING".to_string(),
            args: vec![],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::SimpleString("PONG".to_string()));
    }

    #[test]
    fn test_ping_with_message() {
        let handler = make_handler();
        let cmd = Command {
            name: "PING".to_string(),
            args: vec![b"hello".to_vec()],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::BulkString(Some(b"hello".to_vec())));
    }

    #[test]
    fn test_set_get_del() {
        let handler = make_handler();

        let set_cmd = Command {
            name: "SET".to_string(),
            args: vec![b"mykey".to_vec(), b"myval".to_vec()],
        };
        let resp = handler.handle(&set_cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));

        let get_cmd = Command {
            name: "GET".to_string(),
            args: vec![b"mykey".to_vec()],
        };
        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(Some(b"myval".to_vec())));

        let del_cmd = Command {
            name: "DEL".to_string(),
            args: vec![b"mykey".to_vec()],
        };
        let resp = handler.handle(&del_cmd);
        assert_eq!(resp, RespValue::Integer(1));

        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(None));
    }

    #[test]
    fn test_set_with_ex() {
        let handler = make_handler();
        let set_cmd = Command {
            name: "SET".to_string(),
            args: vec![b"k".to_vec(), b"v".to_vec(), b"EX".to_vec(), b"10".to_vec()],
        };
        let resp = handler.handle(&set_cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));
    }

    #[test]
    fn test_msett_mgett() {
        let handler = make_handler();

        let msett_cmd = Command {
            name: "MSETT".to_string(),
            args: vec![b"mem1".to_vec(), b"data1".to_vec(), b"episodic".to_vec()],
        };
        let resp = handler.handle(&msett_cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));

        let mgett_cmd = Command {
            name: "MGETT".to_string(),
            args: vec![b"mem1".to_vec()],
        };
        let resp = handler.handle(&mgett_cmd);
        match resp {
            RespValue::Array(Some(arr)) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], RespValue::BulkString(Some(b"data1".to_vec())));
                assert_eq!(arr[1], RespValue::BulkString(Some(b"episodic".to_vec())));
                // ttl is some number or -1 for no ttl; episodic has a default ttl from the engine
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn test_mtype() {
        let handler = make_handler();

        let mtype_cmd = Command {
            name: "MTYPE".to_string(),
            args: vec![b"nonexistent".to_vec()],
        };
        let resp = handler.handle(&mtype_cmd);
        assert_eq!(resp, RespValue::SimpleString("none".to_string()));

        let msett_cmd = Command {
            name: "MSETT".to_string(),
            args: vec![b"mem2".to_vec(), b"data2".to_vec(), b"procedural".to_vec()],
        };
        handler.handle(&msett_cmd);

        let mtype_cmd = Command {
            name: "MTYPE".to_string(),
            args: vec![b"mem2".to_vec()],
        };
        let resp = handler.handle(&mtype_cmd);
        assert_eq!(resp, RespValue::SimpleString("procedural".to_string()));
    }

    #[test]
    fn test_unknown_command() {
        let handler = make_handler();
        let cmd = Command {
            name: "FOOBAR".to_string(),
            args: vec![],
        };
        let resp = handler.handle(&cmd);
        match resp {
            RespValue::Error(s) => assert!(s.contains("unknown command")),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn test_set_nx_key_does_not_exist() {
        let handler = make_handler();
        // NX on a non-existent key should set and return OK
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![b"nxkey".to_vec(), b"val".to_vec(), b"NX".to_vec()],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));

        let get_cmd = Command {
            name: "GET".to_string(),
            args: vec![b"nxkey".to_vec()],
        };
        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(Some(b"val".to_vec())));
    }

    #[test]
    fn test_set_nx_key_exists() {
        let handler = make_handler();
        // Set the key first
        let set_cmd = Command {
            name: "SET".to_string(),
            args: vec![b"nxkey2".to_vec(), b"original".to_vec()],
        };
        handler.handle(&set_cmd);
        // NX on an existing key should return nil
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![b"nxkey2".to_vec(), b"newval".to_vec(), b"NX".to_vec()],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::BulkString(None));

        // Value should remain unchanged
        let get_cmd = Command {
            name: "GET".to_string(),
            args: vec![b"nxkey2".to_vec()],
        };
        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(Some(b"original".to_vec())));
    }

    #[test]
    fn test_set_xx_key_does_not_exist() {
        let handler = make_handler();
        // XX on a non-existent key should return nil
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![b"xxkey".to_vec(), b"val".to_vec(), b"XX".to_vec()],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::BulkString(None));

        // Key should not have been set
        let get_cmd = Command {
            name: "GET".to_string(),
            args: vec![b"xxkey".to_vec()],
        };
        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(None));
    }

    #[test]
    fn test_set_xx_key_exists() {
        let handler = make_handler();
        // Set the key first
        let set_cmd = Command {
            name: "SET".to_string(),
            args: vec![b"xxkey2".to_vec(), b"original".to_vec()],
        };
        handler.handle(&set_cmd);
        // XX on an existing key should set and return OK
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![b"xxkey2".to_vec(), b"updated".to_vec(), b"XX".to_vec()],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));

        // Value should be updated
        let get_cmd = Command {
            name: "GET".to_string(),
            args: vec![b"xxkey2".to_vec()],
        };
        let resp = handler.handle(&get_cmd);
        assert_eq!(resp, RespValue::BulkString(Some(b"updated".to_vec())));
    }

    #[test]
    fn test_set_nx_and_xx_mutually_exclusive() {
        let handler = make_handler();
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![
                b"key".to_vec(),
                b"val".to_vec(),
                b"NX".to_vec(),
                b"XX".to_vec(),
            ],
        };
        let resp = handler.handle(&cmd);
        match resp {
            RespValue::Error(s) => assert!(s.contains("mutually exclusive")),
            _ => panic!("expected error for NX+XX"),
        }
    }

    #[test]
    fn test_set_nx_with_ex() {
        let handler = make_handler();
        // NX + EX should work together
        let cmd = Command {
            name: "SET".to_string(),
            args: vec![
                b"nxex".to_vec(),
                b"val".to_vec(),
                b"NX".to_vec(),
                b"EX".to_vec(),
                b"10".to_vec(),
            ],
        };
        let resp = handler.handle(&cmd);
        assert_eq!(resp, RespValue::SimpleString("OK".to_string()));
    }

    #[test]
    fn test_replicaof_no_one() {
        let handler = make_handler();
        let cmd = Command {
            name: "REPLICAOF".to_string(),
            args: vec![b"NO".to_vec(), b"ONE".to_vec()],
        };
        let result = handler.handle_replicaof(&cmd);
        assert!(matches!(result, ReplicaofResult::NoOne));
    }

    #[test]
    fn test_replicaof_host_port() {
        let handler = make_handler();
        let cmd = Command {
            name: "REPLICAOF".to_string(),
            args: vec![b"127.0.0.1".to_vec(), b"6380".to_vec()],
        };
        let result = handler.handle_replicaof(&cmd);
        match result {
            ReplicaofResult::Replicate { host, port } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 6380);
            }
            _ => panic!("expected Replicate"),
        }
    }

    #[test]
    fn test_replicaof_invalid() {
        let handler = make_handler();
        let cmd = Command {
            name: "REPLICAOF".to_string(),
            args: vec![],
        };
        let result = handler.handle_replicaof(&cmd);
        assert!(matches!(result, ReplicaofResult::Error(_)));
    }
}
