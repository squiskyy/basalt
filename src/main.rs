use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

pub mod config;
pub mod http;
pub mod replication;
pub mod resp;
pub mod store;
pub mod time;

#[derive(Parser, Debug)]
#[command(
    name = "basalt",
    version,
    about = "Ultra-high performance KV store for AI memory"
)]
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

    /// Minimum value size (bytes) to LZ4-compress values in memory at runtime (overrides config file, 0 = disable)
    #[arg(long, value_name = "BYTES")]
    compression_threshold: Option<usize>,

    /// WAL size for replication (number of entries, overrides config file)
    #[arg(long, value_name = "N")]
    wal_size: Option<usize>,

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
        Some(path) => match config::Config::from_file(path) {
            Ok(c) => {
                info!("loaded config from {}", path.display());
                c
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
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
        args.compression_threshold,
        args.wal_size,
    );

    // Resolve auth tokens from file + CLI
    let auth_tokens = cfg.resolve_tokens();
    let auth_store = if auth_tokens.is_empty() {
        http::auth::AuthStore::new()
    } else {
        http::auth::AuthStore::from_list(auth_tokens)
    };

    let engine = Arc::new(store::engine::KvEngine::with_max_entries_and_compression(
        cfg.server.shard_count,
        cfg.server.max_entries,
        cfg.server.compression_threshold,
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

    info!(
        "basalt v{} - memory that moves fast",
        env!("CARGO_PKG_VERSION")
    );
    info!("shards: {}", cfg.server.shard_count);
    info!(
        "compression threshold: {} bytes",
        cfg.server.compression_threshold
    );
    if auth.is_enabled() {
        info!("auth: enabled ({} tokens)", auth.list_tokens().len());
    } else {
        info!("auth: disabled (no tokens configured)");
    }
    match &cfg.server.db_path {
        Some(path) => {
            info!(
                "persistence: {} (snapshot every {}ms, compression threshold {} bytes)",
                path, cfg.server.snapshot_interval_ms, cfg.server.snapshot_compression_threshold
            );
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
    if let Some(ref db_path) = cfg.server.db_path
        && cfg.server.snapshot_interval_ms > 0
    {
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

    // Start expired-entry sweep loop if configured
    if cfg.server.sweep_interval_ms > 0 {
        let sweep_engine = engine.clone();
        let sweep_interval = cfg.server.sweep_interval_ms;
        let mut sweep_shutdown = shutdown_rx.clone();
        info!("expired-entry sweep enabled: every {}ms", sweep_interval);
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

    // Create replication state
    let repl_state = Arc::new(replication::ReplicationState::new_primary(
        engine.clone(),
        cfg.server.wal_size,
    ));
    info!(
        "replication: primary mode, WAL size: {} entries",
        cfg.server.wal_size
    );

    let http_repl_state = repl_state.clone();
    let http_shutdown_rx = shutdown_rx.clone();
    let http_server = tokio::spawn(async move {
        let app = http::server::app(
            http_engine,
            http_auth,
            http_config.db_path.clone(),
            http_config.snapshot_compression_threshold,
            Some(http_repl_state),
        );
        let addr = format!("{}:{}", http_config.http_host, http_config.http_port);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        info!("HTTP server listening on {}", addr);
        let mut http_shutdown = http_shutdown_rx.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                http_shutdown.changed().await.ok();
            })
            .await
            .unwrap();
    });

    #[allow(unused_mut)] // mut needed on older tokio where changed() takes &mut self
    let mut resp_shutdown_rx = shutdown_rx.clone();
    let resp_server = tokio::spawn(async move {
        #[cfg(feature = "io-uring")]
        if resp_config.io_uring {
            let host = resp_config.resp_host.clone();
            let port = resp_config.resp_port;
            let db_path = resp_config.db_path.clone();
            let uring_repl_state = repl_state.clone();
            // io_uring runs in its own thread (blocking), separate from tokio
            std::thread::spawn(move || {
                if let Err(e) = resp::uring_server::run(
                    &host,
                    port,
                    resp_engine,
                    resp_auth,
                    db_path,
                    uring_repl_state,
                ) {
                    eprintln!("io_uring RESP server error: {e}");
                }
            });
            // Keep the tokio task alive until shutdown signal
            resp_shutdown_rx.changed().await.ok();
        } else {
            if let Err(e) = resp::server::run_with_replication(
                &resp_config.resp_host,
                resp_config.resp_port,
                resp_engine,
                resp_auth,
                resp_config.db_path.clone(),
                repl_state.clone(),
                resp_shutdown_rx,
            )
            .await
            {
                eprintln!("RESP server error: {e}");
            }
        }

        #[cfg(not(feature = "io-uring"))]
        {
            let rx = resp_shutdown_rx;
            if let Err(e) = resp::server::run_with_replication(
                &resp_config.resp_host,
                resp_config.resp_port,
                resp_engine,
                resp_auth,
                resp_config.db_path.clone(),
                repl_state.clone(),
                rx,
            )
            .await
            {
                eprintln!("RESP server error: {e}");
            }
        }
    });

    // Handle graceful shutdown via SIGINT/SIGTERM.
    // Both the HTTP and RESP servers observe the shutdown watch channel,
    // so sending true on the channel will signal them both to stop.
    let shutdown_tx_clone = shutdown_tx.clone();
    let ctrl_c = tokio::signal::ctrl_c();
    let terminate = async {
        #[cfg(unix)]
        {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    };

    tokio::spawn(async move {
        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received Ctrl+C, initiating graceful shutdown");
            }
            _ = terminate => {
                tracing::info!("received SIGTERM, initiating graceful shutdown");
            }
        }
        let _ = shutdown_tx_clone.send(true);
    });

    // Wait for either server to finish (they will stop once shutdown is signaled).
    tokio::select! {
        _ = http_server => tracing::info!("HTTP server stopped"),
        _ = resp_server => tracing::info!("RESP server stopped"),
    }

    // Ensure the shutdown signal is sent to the snapshot/sweep loops too.
    let _ = shutdown_tx.send(true);
}
