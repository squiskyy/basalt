use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

pub mod config;
pub mod http;
pub mod resp;
pub mod store;

#[derive(Parser, Debug)]
#[command(name = "basalt", version, about = "Ultra-high performance KV store for AI memory")]
struct Args {
    /// Path to TOML config file
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// HTTP host to bind to (overrides config file)
    #[arg(long)]
    http_host: Option<String>,

    /// HTTP port for the REST API (overrides config file)
    #[arg(long)]
    http_port: Option<u16>,

    /// RESP host to bind to (overrides config file)
    #[arg(long)]
    resp_host: Option<String>,

    /// RESP port (Redis-compatible protocol) (overrides config file)
    #[arg(long)]
    resp_port: Option<u16>,

    /// Number of shards (overrides config file)
    #[arg(long)]
    shards: Option<usize>,

    /// Auth tokens (format: "token:ns1,ns2" or "token:*" for all).
    /// Overrides tokens from config file. Can be specified multiple times.
    #[arg(long, value_name = "TOKEN")]
    auth: Vec<String>,

    /// Path to auth tokens file (one token per line: "token namespace1 [namespace2 ...]")
    /// Overrides tokens_file from config file.
    #[arg(long, value_name = "FILE")]
    auth_file: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("basalt=info")),
        )
        .init();

    let args = Args::parse();

    // Load config: start with file (if provided), then apply CLI overrides
    let mut cfg = match &args.config {
        Some(path) => {
            match config::Config::from_file(path) {
                Ok(c) => {
                    info!("loaded config from {}", path.display());
                    c
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => config::Config::default(),
    };

    // Apply CLI overrides
    cfg.apply_cli_overrides(
        args.http_host,
        args.http_port,
        args.resp_host,
        args.resp_port,
        args.shards,
        args.auth,
        args.auth_file,
    );

    // Resolve auth tokens from file + CLI
    let auth_tokens = cfg.resolve_tokens();
    let auth_store = if auth_tokens.is_empty() {
        http::auth::AuthStore::new()
    } else {
        http::auth::AuthStore::from_list(auth_tokens)
    };

    let engine = Arc::new(store::engine::KvEngine::new(cfg.server.shard_count));
    let auth = Arc::new(auth_store);

    info!("basalt v{} — memory that moves fast", env!("CARGO_PKG_VERSION"));
    info!("shards: {}", cfg.server.shard_count);
    if auth.is_enabled() {
        info!("auth: enabled ({} tokens)", auth.list_tokens().len());
    } else {
        info!("auth: disabled (no tokens configured)");
    }
    info!("HTTP:  {}:{}", cfg.server.http_host, cfg.server.http_port);
    info!("RESP:  {}:{}", cfg.server.resp_host, cfg.server.resp_port);

    // Start both servers concurrently
    let http_engine = engine.clone();
    let http_auth = auth.clone();
    let resp_engine = engine.clone();
    let resp_auth = auth.clone();
    let http_config = cfg.server.clone();
    let resp_config = cfg.server.clone();

    let http_server = tokio::spawn(async move {
        let app = http::server::app(http_engine, http_auth);
        let addr = format!("{}:{}", http_config.http_host, http_config.http_port);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        info!("HTTP server listening on {}", addr);
        axum::serve(listener, app).await.unwrap();
    });

    let resp_server = tokio::spawn(async move {
        if let Err(e) = resp::server::run(
            &resp_config.resp_host,
            resp_config.resp_port,
            resp_engine,
            resp_auth,
        )
        .await
        {
            eprintln!("RESP server error: {e}");
        }
    });

    tokio::select! {
        _ = http_server => info!("HTTP server stopped"),
        _ = resp_server => info!("RESP server stopped"),
    }
}
