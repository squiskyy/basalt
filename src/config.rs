/// Configuration for the Basalt server.
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTP server bind address.
    pub http_host: String,
    /// HTTP server port.
    pub http_port: u16,
    /// RESP server bind address.
    pub resp_host: String,
    /// RESP server port (Redis-compatible).
    pub resp_port: u16,
    /// Number of shards in the KV engine (rounded to power of 2).
    pub shard_count: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_host: "127.0.0.1".into(),
            http_port: 7380,
            resp_host: "127.0.0.1".into(),
            resp_port: 6380,
            shard_count: 64,
        }
    }
}
