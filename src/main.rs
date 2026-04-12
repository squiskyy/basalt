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

    /// Use io_uring for RESP server (Linux only, requires --features io-uring)
    #[cfg(feature = "io-uring")]
    #[arg(long)]
    io_uring: bool,

    /// Directory for persistence snapshots (overrides config file)
    #[arg(long, value_name = "DIR")]
    db_path: Option<String>,

    /// Auto-snapshot interval in milliseconds (overrides config file, 0 = disabled)
    #[arg(long, value_name = "MS")]
    snapshot_interval: Option<u64>,

    /// Expired-entry sweep interval in milliseconds (overrides config file, 0 = disabled)
    #[arg(long, value_name = "MS")]
    sweep_interval: Option<u64>,

    /// Maximum entries per shard (overrides config file, rejects writes when at capacity)
    #[arg(long, value_name = "N")]
    max_entries: Option<usize>,

    /// Minimum value size (bytes) to LZ4-compress in snapshots (overrides config file, 0 = disable)
    #[arg(long, value_name = "BYTES")]
    snapshot_compression_threshold: Option<usize>,

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
        #[cfg(feature = "io-uring")]
        args.io_uring.then_some(true),
        args.db_path,
        args.snapshot_interval,
        args.sweep_interval,
        args.auth,
        args.auth_file,
        args.max_entries,
        args.snapshot_compression_threshold,
    );

    // Resolve auth tokens from file + CLI
    let auth_tokens = cfg.resolve_tokens();
    let auth_store = if auth_tokens.is_empty() {
        http::auth::AuthStore::new()
    } else {
        http::auth::AuthStore::from_list(auth_tokens)
    };

    let engine = Arc::new(store::engine::KvEngine::with_max_entries(
        cfg.server.shard_count,
        cfg.server.max_entries,
    ));
    let auth = Arc::new(auth_store);

    // Load snapshot if db_path is configured
    if let Some(ref db_path) = cfg.server.db_path {
        let db_path = std::path::Path::new(db_path);
        match store::persistence::load_latest_snapshot(db_path, &engine) {
            Ok(count) => info!("restored {count} entries from snapshot"),
            Err(e) => {
                eprintln!("warning: failed to load snapshot: {e}");
            }
        }
    }

    info!("basalt v{} — memory that moves fast", env!("CARGO_PKG_VERSION"));
    info!("shards: {}", cfg.server.shard_count);
    if auth.is_enabled() {
        info!("auth: enabled ({} tokens)", auth.list_tokens().len());
    } else {
        info!("auth: disabled (no tokens configured)");
    }
    match &cfg.server.db_path {
        Some(path) => {
            info!("persistence: {} (snapshot every {}ms, compression threshold {} bytes)", path, cfg.server.snapshot_interval_ms, cfg.server.snapshot_compression_threshold);
        }
        None => info!("persistence: disabled (no db_path)"),
    }
    info!("HTTP:  {}:{}", cfg.server.http_host, cfg.server.http_port);
    info!("RESP:  {}:{}", cfg.server.resp_host, cfg.server.resp_port);
    #[cfg(feature = "io-uring")]
    if cfg.server.io_uring {
        info!("RESP backend: io_uring (raw)");
    } else {
        info!("RESP backend: tokio");
    }

    // Start auto-snapshot loop if configured
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    if let Some(ref db_path) = cfg.server.db_path {
        if cfg.server.snapshot_interval_ms > 0 {
            let snap_db_path = std::path::PathBuf::from(db_path);
            let snap_engine = engine.clone();
            let snap_interval = cfg.server.snapshot_interval_ms;
            let snap_threshold = cfg.server.snapshot_compression_threshold;
            tokio::spawn(store::persistence::start_snapshot_loop_with_threshold(
                snap_db_path,
                snap_engine,
                snap_interval,
                3, // keep last 3 snapshots
                snap_threshold,
                shutdown_rx.clone(),
            ));
        }
    }

    // Start expired-entry sweep loop if configured
    if cfg.server.sweep_interval_ms > 0 {
        let sweep_engine = engine.clone();
        let sweep_interval = cfg.server.sweep_interval_ms;
        let mut sweep_shutdown = shutdown_rx.clone();
        info!(
            "expired-entry sweep enabled: every {}ms",
            sweep_interval
        );
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(sweep_interval)) => {
                        let reaped = sweep_engine.reap_all_expired().await;
                        if reaped > 0 {
                            tracing::debug!("sweep: reaped {reaped} expired entries");
                        }
                    }
                    _ = sweep_shutdown.changed() => {
                        tracing::info!("expired-entry sweep shutting down");
                        break;
                    }
                }
            }
        });
    } else {
        info!("expired-entry sweep: disabled (interval = 0)");
    }

    // Start both servers concurrently
    let http_engine = engine.clone();
    let http_auth = auth.clone();
    let resp_engine = engine.clone();
    let resp_auth = auth.clone();
    let http_config = cfg.server.clone();
    let resp_config = cfg.server.clone();

    let http_server = tokio::spawn(async move {
        let app = http::server::app(http_engine, http_auth, http_config.db_path.clone());
        let addr = format!("{}:{}", http_config.http_host, http_config.http_port);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        info!("HTTP server listening on {}", addr);
        axum::serve(listener, app).await.unwrap();
    });

    let resp_server = tokio::spawn(async move {
        #[cfg(feature = "io-uring")]
        if resp_config.io_uring {
            let host = resp_config.resp_host.clone();
            let port = resp_config.resp_port;
            let db_path = resp_config.db_path.clone();
            // io_uring runs in its own thread (blocking), separate from tokio
            std::thread::spawn(move || {
                if let Err(e) = resp::uring_server::run(&host, port, resp_engine, resp_auth, db_path) {
                    eprintln!("io_uring RESP server error: {e}");
                }
            });
            // Keep the tokio task alive
            std::future::pending::<()>().await;
        } else {
            if let Err(e) = resp::server::run(
                &resp_config.resp_host,
                resp_config.resp_port,
                resp_engine,
                resp_auth,
                resp_config.db_path.clone(),
            )
            .await
            {
                eprintln!("RESP server error: {e}");
            }
        }

        #[cfg(not(feature = "io-uring"))]
        if let Err(e) = resp::server::run(
            &resp_config.resp_host,
            resp_config.resp_port,
            resp_engine,
            resp_auth,
            resp_config.db_path.clone(),
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

    // Signal shutdown to snapshot loop
    let _ = shutdown_tx.send(true);
}
