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
    pub llm: LlmConfig,
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
    /// How often to sweep expired entries (milliseconds). 0 = disabled.
    pub sweep_interval_ms: u64,
    /// Maximum entries per shard. Rejects new entries when at capacity.
    pub max_entries: usize,
    /// Minimum value size (bytes) to LZ4-compress in snapshots. 0 = disable compression.
    pub snapshot_compression_threshold: usize,
    /// Minimum value size (bytes) to LZ4-compress values in memory (runtime). 0 = disable.
    pub compression_threshold: usize,
    /// WAL size for replication (number of entries to keep). Default 10000.
    pub wal_size: usize,
    /// Eviction policy when a shard hits max_entries capacity.
    /// "reject" = reject writes (default), "ttl-first" = sweep expired then reject, "lru" = evict least-recently-used
    pub eviction: String,
    /// Enable automatic failover when primary lease expires.
    pub failover: bool,
    /// How long (ms) without a lease heartbeat before considering the primary dead.
    pub failover_timeout_ms: u64,
    /// Known peer addresses for failover reconfiguration (host:port, comma-separated).
    pub replica_peers: Vec<String>,
    /// Path to TLS certificate file (PEM format). Requires a TLS feature flag.
    pub tls_cert: Option<String>,
    /// Path to TLS private key file (PEM format). Requires a TLS feature flag.
    pub tls_key: Option<String>,
    /// How often (ms) to reap low-relevance entries. 0 = disabled.
    pub decay_reap_interval_ms: u64,
    /// Default decay lambda (rate parameter). Default: ln(2)/24 (~0.0289) for 24h halflife.
    pub decay_default_lambda: f64,
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
            sweep_interval_ms: 30_000,
            max_entries: 1_000_000,
            snapshot_compression_threshold: 1024,
            compression_threshold: 1024,
            wal_size: 10_000,
            eviction: "reject".to_string(),
            failover: false,
            failover_timeout_ms: 15_000,
            replica_peers: Vec::new(),
            tls_cert: None,
            tls_key: None,
            decay_reap_interval_ms: 0,
            decay_default_lambda: std::f64::consts::LN_2 / 24.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Path to auth tokens file (one token per line).
    pub tokens_file: Option<String>,
    /// Tokens specified directly (from CLI --auth flags).
    pub tokens: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// LLM provider: "openai", "anthropic", or "custom".
    /// Empty = LLM disabled.
    pub provider: String,
    /// API key for the LLM provider.
    pub api_key: String,
    /// Model to use (provider-specific).
    pub model: String,
    /// Base URL override (for OpenAI-compatible endpoints like Ollama, vLLM).
    /// Empty = use provider default.
    pub base_url: String,
    /// API version header (Anthropic only).
    pub api_version: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Default max tokens for completions.
    pub default_max_tokens: u32,
    /// Default temperature for completions.
    pub default_temperature: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            api_key: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_version: "2023-06-01".into(),
            timeout_ms: 30_000,
            default_max_tokens: 1024,
            default_temperature: 0.3,
        }
    }
}

impl LlmConfig {
    /// Check if LLM is enabled (provider is set).
    pub fn is_enabled(&self) -> bool {
        !self.provider.is_empty()
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
    #[serde(default)]
    llm: LlmFile,
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
    #[serde(default)]
    sweep_interval_ms: Option<u64>,
    #[serde(default)]
    max_entries: Option<usize>,
    #[serde(default)]
    snapshot_compression_threshold: Option<usize>,
    #[serde(default)]
    compression_threshold: Option<usize>,
    #[serde(default)]
    wal_size: Option<usize>,
    #[serde(default)]
    eviction: Option<String>,
    #[serde(default)]
    failover: Option<bool>,
    #[serde(default)]
    failover_timeout_ms: Option<u64>,
    #[serde(default)]
    replica_peers: Option<Vec<String>>,
    #[serde(default)]
    tls_cert: Option<String>,
    #[serde(default)]
    tls_key: Option<String>,
    #[serde(default)]
    decay_reap_interval_ms: Option<u64>,
    #[serde(default)]
    decay_default_lambda: Option<f64>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct AuthFile {
    #[serde(default)]
    tokens_file: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct LlmFile {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_version: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    default_max_tokens: Option<u32>,
    #[serde(default)]
    default_temperature: Option<f32>,
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
            shard_count: file
                .server
                .shard_count
                .unwrap_or(server_defaults.shard_count),
            #[cfg(feature = "io-uring")]
            io_uring: file.server.io_uring.unwrap_or(false),
            db_path: file.server.db_path,
            snapshot_interval_ms: file
                .server
                .snapshot_interval_ms
                .unwrap_or(server_defaults.snapshot_interval_ms),
            sweep_interval_ms: file
                .server
                .sweep_interval_ms
                .unwrap_or(server_defaults.sweep_interval_ms),
            max_entries: file
                .server
                .max_entries
                .unwrap_or(server_defaults.max_entries),
            snapshot_compression_threshold: file
                .server
                .snapshot_compression_threshold
                .unwrap_or(server_defaults.snapshot_compression_threshold),
            compression_threshold: file
                .server
                .compression_threshold
                .unwrap_or(server_defaults.compression_threshold),
            wal_size: file.server.wal_size.unwrap_or(server_defaults.wal_size),
            eviction: file
                .server
                .eviction
                .unwrap_or_else(|| server_defaults.eviction.clone()),
            failover: file.server.failover.unwrap_or(false),
            failover_timeout_ms: file
                .server
                .failover_timeout_ms
                .unwrap_or(server_defaults.failover_timeout_ms),
            replica_peers: file.server.replica_peers.unwrap_or_default(),
            tls_cert: file.server.tls_cert,
            tls_key: file.server.tls_key,
            decay_reap_interval_ms: file
                .server
                .decay_reap_interval_ms
                .unwrap_or(server_defaults.decay_reap_interval_ms),
            decay_default_lambda: file
                .server
                .decay_default_lambda
                .unwrap_or(server_defaults.decay_default_lambda),
        };

        let auth = AuthConfig {
            tokens_file: file.auth.tokens_file,
            tokens: Vec::new(),
        };

        let llm_defaults = LlmConfig::default();
        let llm = LlmConfig {
            provider: file.llm.provider.unwrap_or_default(),
            api_key: file.llm.api_key.unwrap_or_default(),
            model: file.llm.model.unwrap_or_default(),
            base_url: file.llm.base_url.unwrap_or_default(),
            api_version: file.llm.api_version.unwrap_or(llm_defaults.api_version),
            timeout_ms: file.llm.timeout_ms.unwrap_or(llm_defaults.timeout_ms),
            default_max_tokens: file
                .llm
                .default_max_tokens
                .unwrap_or(llm_defaults.default_max_tokens),
            default_temperature: file
                .llm
                .default_temperature
                .unwrap_or(llm_defaults.default_temperature),
        };

        Ok(Config { server, auth, llm })
    }

    /// Apply CLI arg overrides on top of the loaded config.
    /// Only overwrites values that were explicitly set via CLI.
    #[allow(clippy::too_many_arguments)]
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
        sweep_interval_ms: Option<u64>,
        auth_tokens: Vec<String>,
        auth_tokens_file: Option<String>,
        max_entries: Option<usize>,
        snapshot_compression_threshold: Option<usize>,
        compression_threshold: Option<usize>,
        wal_size: Option<usize>,
        eviction: Option<String>,
        failover: bool,
        failover_timeout: Option<u64>,
        replica_peers: Option<Vec<String>>,
        tls_cert: Option<String>,
        tls_key: Option<String>,
        llm_provider: Option<String>,
        llm_api_key: Option<String>,
        llm_model: Option<String>,
        llm_base_url: Option<String>,
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
        if let Some(v) = sweep_interval_ms {
            self.server.sweep_interval_ms = v;
        }
        if !auth_tokens.is_empty() {
            self.auth.tokens = auth_tokens;
        }
        if let Some(v) = auth_tokens_file {
            self.auth.tokens_file = Some(v);
        }
        if let Some(v) = max_entries {
            self.server.max_entries = v;
        }
        if let Some(v) = snapshot_compression_threshold {
            self.server.snapshot_compression_threshold = v;
        }
        if let Some(v) = compression_threshold {
            self.server.compression_threshold = v;
        }
        if let Some(v) = wal_size {
            self.server.wal_size = v;
        }
        if let Some(v) = eviction {
            self.server.eviction = v;
        }
        self.server.failover = failover;
        if let Some(v) = failover_timeout {
            self.server.failover_timeout_ms = v;
        }
        if let Some(v) = replica_peers {
            self.server.replica_peers = v;
        }
        if let Some(v) = tls_cert {
            self.server.tls_cert = Some(v);
        }
        if let Some(v) = tls_key {
            self.server.tls_key = Some(v);
        }
        if let Some(v) = llm_provider {
            self.llm.provider = v;
        }
        if let Some(v) = llm_api_key {
            self.llm.api_key = v;
        }
        if let Some(v) = llm_model {
            self.llm.model = v;
        }
        if let Some(v) = llm_base_url {
            self.llm.base_url = v;
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
        assert_eq!(config.server.max_entries, 1_000_000);
        assert_eq!(config.server.compression_threshold, 1024);
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
            Some(5_000u64), // sweep_interval_ms
            vec!["bsk-test:*".to_string()],
            Some("/path/to/tokens".to_string()),
            Some(500_000), // max_entries
            None,          // snapshot_compression_threshold
            None,          // compression_threshold
            None,          // wal_size
            None,          // eviction
            false,         // failover
            None,          // failover_timeout
            None,          // replica_peers
            None,          // tls_cert
            None,          // tls_key
            None,          // llm_provider
            None,          // llm_api_key
            None,          // llm_model
            None,          // llm_base_url
        );
        assert_eq!(config.server.http_host, "0.0.0.0");
        assert_eq!(config.server.http_port, 9000);
        assert_eq!(config.server.resp_host, "127.0.0.1"); // unchanged
        assert_eq!(config.server.shard_count, 32);
        assert_eq!(config.server.db_path, Some("/data/basalt".to_string()));
        assert_eq!(config.server.snapshot_interval_ms, 10_000);
        assert_eq!(config.server.sweep_interval_ms, 5_000);
        assert_eq!(config.server.max_entries, 500_000);
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
        writeln!(f).unwrap();
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
