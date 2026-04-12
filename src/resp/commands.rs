use std::sync::Arc;

use crate::store::engine::KvEngine;
use crate::store::memory_type::MemoryType;

use super::parser::{Command, RespValue};

/// Handles RESP commands by dispatching to the KvEngine.
pub struct CommandHandler {
    engine: Arc<KvEngine>,
}

impl CommandHandler {
    pub fn new(engine: Arc<KvEngine>) -> Self {
        CommandHandler { engine }
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
            // AUTH is handled separately in the connection handler
            "AUTH" => RespValue::Error("ERR AUTH already handled at connection level".to_string()),
            _ => RespValue::Error(format!("ERR unknown command '{}'", cmd.name)),
        }
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
        // SET key value [EX seconds] [PX milliseconds]
        if cmd.args.len() < 2 {
            return RespValue::Error("ERR wrong number of arguments for 'SET'".to_string());
        }

        let key = String::from_utf8_lossy(&cmd.args[0]).to_string();
        let value = cmd.args[1].clone();
        let mut ttl_ms: Option<u64> = None;

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
                            )
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
                            )
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

        self.engine.set(&key, value, ttl_ms, MemoryType::Semantic);
        RespValue::SimpleString("OK".to_string())
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
        if cmd.args.len() < 2 || cmd.args.len() % 2 != 0 {
            return RespValue::Error("ERR wrong number of arguments for 'MSET'".to_string());
        }
        let mut i = 0;
        while i < cmd.args.len() {
            let key = String::from_utf8_lossy(&cmd.args[i]).to_string();
            let value = cmd.args[i + 1].clone();
            self.engine.set(&key, value, None, MemoryType::Semantic);
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

    fn handle_info(&self, cmd: &Command) -> RespValue {
        // INFO optionally takes a section name; we ignore it and return everything
        let _ = cmd; // suppress unused warning
        let info = format!(
            "# Basalt\r\nbasalt_version:0.1.0\r\nshard_count:{}\r\n",
            self.engine.shard_count()
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
                )
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
                            )
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

        self.engine.set(&key, value, ttl_ms, memory_type);
        RespValue::SimpleString("OK".to_string())
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler() -> CommandHandler {
        CommandHandler::new(Arc::new(KvEngine::new(4)))
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
}
