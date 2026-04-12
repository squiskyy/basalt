/// Configuration for the Basalt server.
///
/// Can be loaded from a TOML config file or built from CLI args.
/// CLI args override config file values.

use std::fs;
use std::path::Path;
use tracing::warn;

/// Top-level config structure matching the TOML file format.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// HTTP server bind address.
    pub http_host: String,
    /// HTTP server port.
    pub http_port: u16,
    /// RESP server bind address.
    pub resp_host: String,
    /// RESP server port (Redis-compatible).
    pub resp_port: u16,
    /// Number of shards in the KV engine.
    pub shard_count: usize,
    /// Use io_uring for the RESP server (Linux only).
    #[cfg(feature = "io-uring")]
    pub io_uring: bool,
    /// Directory for persistence snapshots. None = no persistence.
    pub db_path: Option<String>,
    /// How often to auto-snapshot to disk (milliseconds). 0 = disabled.
    pub snapshot_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Path to auth tokens file (one token per line).
    pub tokens_file: Option<String>,
    /// Tokens specified directly (from CLI --auth flags).
    pub tokens: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_host: "127.0.0.1".into(),
            http_port: 7380,
            resp_host: "127.0.0.1".into(),
            resp_port: 6380,
            shard_count: 64,
            #[cfg(feature = "io-uring")]
            io_uring: false,
            db_path: None,
            snapshot_interval_ms: 60_000,
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            tokens_file: None,
            tokens: Vec::new(),
        }
    }
}

/// TOML-deserializable config structure.
/// Uses Option fields so missing keys fall back to defaults.
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    server: ServerFile,
    #[serde(default)]
    auth: AuthFile,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct ServerFile {
    #[serde(default)]
    http_host: Option<String>,
    #[serde(default)]
    http_port: Option<u16>,
    #[serde(default)]
    resp_host: Option<String>,
    #[serde(default)]
    resp_port: Option<u16>,
    #[serde(default)]
    shard_count: Option<usize>,
    #[cfg(feature = "io-uring")]
    #[serde(default)]
    io_uring: Option<bool>,
    #[serde(default)]
    db_path: Option<String>,
    #[serde(default)]
    snapshot_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct AuthFile {
    #[serde(default)]
    tokens_file: Option<String>,
}

impl Config {
    /// Load config from a TOML file. Missing keys use defaults.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let contents = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config file {}: {e}", path.display()))?;

        let file: ConfigFile = toml::from_str(&contents)
            .map_err(|e| format!("failed to parse config file {}: {e}", path.display()))?;

        let server_defaults = ServerConfig::default();
        let server = ServerConfig {
            http_host: file.server.http_host.unwrap_or(server_defaults.http_host),
            http_port: file.server.http_port.unwrap_or(server_defaults.http_port),
            resp_host: file.server.resp_host.unwrap_or(server_defaults.resp_host),
            resp_port: file.server.resp_port.unwrap_or(server_defaults.resp_port),
            shard_count: file.server.shard_count.unwrap_or(server_defaults.shard_count),
            #[cfg(feature = "io-uring")]
            io_uring: file.server.io_uring.unwrap_or(false),
            db_path: file.server.db_path,
            snapshot_interval_ms: file
                .server
                .snapshot_interval_ms
                .unwrap_or(server_defaults.snapshot_interval_ms),
        };

        let auth = AuthConfig {
            tokens_file: file.auth.tokens_file,
            tokens: Vec::new(),
        };

        Ok(Config { server, auth })
    }

    /// Apply CLI arg overrides on top of the loaded config.
    /// Only overwrites values that were explicitly set via CLI.
    pub fn apply_cli_overrides(
        &mut self,
        http_host: Option<String>,
        http_port: Option<u16>,
        resp_host: Option<String>,
        resp_port: Option<u16>,
        shard_count: Option<usize>,
        #[cfg(feature = "io-uring")] io_uring: Option<bool>,
        db_path: Option<String>,
        snapshot_interval_ms: Option<u64>,
        auth_tokens: Vec<String>,
        auth_tokens_file: Option<String>,
    ) {
        if let Some(v) = http_host {
            self.server.http_host = v;
        }
        if let Some(v) = http_port {
            self.server.http_port = v;
        }
        if let Some(v) = resp_host {
            self.server.resp_host = v;
        }
        if let Some(v) = resp_port {
            self.server.resp_port = v;
        }
        if let Some(v) = shard_count {
            self.server.shard_count = v;
        }
        #[cfg(feature = "io-uring")]
        if let Some(v) = io_uring {
            self.server.io_uring = v;
        }
        if let Some(v) = db_path {
            self.server.db_path = Some(v);
        }
        if let Some(v) = snapshot_interval_ms {
            self.server.snapshot_interval_ms = v;
        }
        if !auth_tokens.is_empty() {
            self.auth.tokens = auth_tokens;
        }
        if let Some(v) = auth_tokens_file {
            self.auth.tokens_file = Some(v);
        }
    }

    /// Load auth tokens from the tokens file + CLI tokens.
    /// Returns a list of (token_value, namespaces) pairs.
    pub fn resolve_tokens(&self) -> Vec<(String, Vec<String>)> {
        let mut tokens: Vec<(String, Vec<String>)> = Vec::new();

        // Load from file first
        if let Some(ref path) = self.auth.tokens_file {
            match load_tokens_file(Path::new(path)) {
                Ok(file_tokens) => {
                    for (token, namespaces) in file_tokens {
                        tokens.push((token, namespaces));
                    }
                }
                Err(e) => {
                    warn!("failed to load tokens file: {e}");
                }
            }
        }

        // Then load from CLI --auth flags (these override file entries with same token value)
        for spec in &self.auth.tokens {
            if let Some((token, namespaces)) = parse_token_spec(spec) {
                // Remove existing entry with same token if present
                tokens.retain(|(t, _)| t != &token);
                tokens.push((token, namespaces));
            }
        }

        tokens
    }
}

/// Parse a CLI auth token spec: "bsk-abc123:*" or "bsk-agent1:agent-1,shared"
pub fn parse_token_spec(spec: &str) -> Option<(String, Vec<String>)> {
    let (token, namespaces_str) = spec.split_once(':')?;
    let token = token.trim().to_string();
    let namespaces = namespaces_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if token.is_empty() || namespaces.is_empty() {
        return None;
    }
    Some((token, namespaces))
}

/// Auth tokens file format:
///
/// ```text
/// # Lines starting with # are comments
/// # Blank lines are ignored
///
/// # Format: token namespaces
/// # One token per line, namespaces are space-separated
/// # Use * for wildcard (all namespaces)
///
/// bsk-admin-abc123 *
/// bsk-agent1-def456 agent-1 shared
/// bsk-agent2-ghi789 agent-2
///
/// # Token with a single namespace
/// bsk-readonly readonly-ns
/// ```
///
/// Each line: `<token> <namespace1> [<namespace2> ...]`
/// The token is the first whitespace-delimited field.
/// Remaining fields are the namespaces the token can access.
fn load_tokens_file(path: &Path) -> Result<Vec<(String, Vec<String>)>, String> {
    let contents = fs::read_to_string(path)
        .map_err(|e| format!("failed to read tokens file {}: {e}", path.display()))?;

    let mut tokens = Vec::new();

    for (line_num, line) in contents.lines().enumerate() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on whitespace: first field is token, rest are namespaces
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(format!(
                "tokens file {} line {}: expected at least 2 fields (token + namespace), got {}",
                path.display(),
                line_num + 1,
                parts.len()
            ));
        }

        let token = parts[0].to_string();
        let namespaces: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        if namespaces.is_empty() {
            return Err(format!(
                "tokens file {} line {}: token '{}' has no namespaces",
                path.display(),
                line_num + 1,
                token
            ));
        }

        tokens.push((token, namespaces));
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.server.http_port, 7380);
        assert_eq!(config.server.resp_port, 6380);
        assert_eq!(config.server.shard_count, 64);
        assert!(config.server.db_path.is_none());
        assert_eq!(config.server.snapshot_interval_ms, 60_000);
        assert!(config.auth.tokens_file.is_none());
        assert!(config.auth.tokens.is_empty());
    }

    #[test]
    fn test_config_from_toml() {
        let toml = r#"
[server]
http_port = 8080
resp_port = 6379
shard_count = 128
db_path = "/var/lib/basalt"
snapshot_interval_ms = 30000

[auth]
tokens_file = "/etc/basalt/tokens.txt"
"#;
        let dir = std::env::temp_dir().join("basalt_test_config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("basalt.toml");
        std::fs::write(&path, toml).unwrap();

        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.server.http_port, 8080);
        assert_eq!(config.server.resp_port, 6379);
        assert_eq!(config.server.shard_count, 128);
        assert_eq!(config.server.http_host, "127.0.0.1"); // default
        assert_eq!(config.server.db_path, Some("/var/lib/basalt".to_string()));
        assert_eq!(config.server.snapshot_interval_ms, 30_000);
        assert_eq!(
            config.auth.tokens_file,
            Some("/etc/basalt/tokens.txt".to_string())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_config_partial_toml() {
        let toml = r#"
[server]
http_port = 9999
"#;
        let dir = std::env::temp_dir().join("basalt_test_partial");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("basalt.toml");
        std::fs::write(&path, toml).unwrap();

        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.server.http_port, 9999);
        assert_eq!(config.server.resp_port, 6380); // default
        assert!(config.server.db_path.is_none()); // default
        assert_eq!(config.server.snapshot_interval_ms, 60_000); // default
        assert!(config.auth.tokens_file.is_none()); // default

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cli_overrides() {
        let mut config = Config::default();
        config.apply_cli_overrides(
            Some("0.0.0.0".to_string()),
            Some(9000u16),
            None,
            None,
            Some(32),
            #[cfg(feature = "io-uring")]
            None, // io_uring
            Some("/data/basalt".to_string()),
            Some(10_000u64),
            vec!["bsk-test:*".to_string()],
            Some("/path/to/tokens".to_string()),
        );
        assert_eq!(config.server.http_host, "0.0.0.0");
        assert_eq!(config.server.http_port, 9000);
        assert_eq!(config.server.resp_host, "127.0.0.1"); // unchanged
        assert_eq!(config.server.shard_count, 32);
        assert_eq!(config.server.db_path, Some("/data/basalt".to_string()));
        assert_eq!(config.server.snapshot_interval_ms, 10_000);
        assert_eq!(config.auth.tokens, vec!["bsk-test:*"]);
        assert_eq!(config.auth.tokens_file, Some("/path/to/tokens".to_string()));
    }

    #[test]
    fn test_parse_token_spec() {
        assert_eq!(
            parse_token_spec("bsk-admin:*"),
            Some(("bsk-admin".to_string(), vec!["*".to_string()]))
        );
        assert_eq!(
            parse_token_spec("bsk-agent1:agent-1,shared"),
            Some((
                "bsk-agent1".to_string(),
                vec!["agent-1".to_string(), "shared".to_string()]
            ))
        );
        assert_eq!(parse_token_spec("no-colon"), None);
        assert_eq!(parse_token_spec("token:"), None); // empty namespaces
        assert_eq!(parse_token_spec(":ns"), None); // empty token
    }

    #[test]
    fn test_load_tokens_file() {
        let dir = std::env::temp_dir().join("basalt_test_tokens");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.txt");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# Admin token - full access").unwrap();
        writeln!(f, "bsk-admin *").unwrap();
        writeln!(f, "").unwrap();
        writeln!(f, "# Agent 1 - scoped access").unwrap();
        writeln!(f, "bsk-agent1 agent-1 shared").unwrap();
        writeln!(f, "bsk-agent2 agent-2").unwrap();

        let tokens = load_tokens_file(&path).unwrap();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], ("bsk-admin".to_string(), vec!["*".to_string()]));
        assert_eq!(
            tokens[1],
            (
                "bsk-agent1".to_string(),
                vec!["agent-1".to_string(), "shared".to_string()]
            )
        );
        assert_eq!(
            tokens[2],
            ("bsk-agent2".to_string(), vec!["agent-2".to_string()])
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_tokens_file_invalid() {
        let dir = std::env::temp_dir().join("basalt_test_tokens_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.txt");
        std::fs::write(&path, "lonely-token-no-namespaces\n").unwrap();

        let result = load_tokens_file(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("expected at least 2 fields"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resolve_tokens_merges_file_and_cli() {
        let dir = std::env::temp_dir().join("basalt_test_resolve");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.txt");
        std::fs::write(&path, "bsk-file-token *\nbsk-override agent-1\n").unwrap();

        let mut config = Config::default();
        config.auth.tokens_file = Some(path.to_string_lossy().to_string());
        config.auth.tokens = vec!["bsk-override:agent-2,shared".to_string()];

        let tokens = config.resolve_tokens();
        // bsk-file-token from file, bsk-override from CLI (overrides file entry)
        assert_eq!(tokens.len(), 2);
        // CLI override replaces file entry for bsk-override
        let override_entry = tokens.iter().find(|(t, _)| t == "bsk-override").unwrap();
        assert_eq!(override_entry.1, vec!["agent-2", "shared"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
