use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use basalt::config;
use basalt::http;
use basalt::metrics;
use basalt::replication;
use basalt::resp;
use basalt::store;
use basalt::store::share::ShareStore;

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

    /// Eviction policy when shards hit capacity: reject, ttl-first, or lru (overrides config file)
    #[arg(long, value_name = "POLICY")]
    eviction: Option<String>,

    /// Enable automatic failover when primary lease expires.
    /// Replicas will self-promote to primary if the primary becomes unreachable.
    #[arg(long)]
    failover: bool,

    /// Failover timeout in milliseconds - how long without a lease heartbeat
    /// before considering the primary dead (default: 15000).
    #[arg(long, value_name = "MS")]
    failover_timeout: Option<u64>,

    /// Known replica peer addresses for failover reconfiguration (comma-separated host:port).
    /// When the primary connection is lost, replicas try these peers to find a new primary.
    #[arg(long, value_name = "PEERS")]
    replica_peers: Option<String>,

    /// Path to TLS certificate file (PEM format) for RESP protocol encryption.
    /// Requires a TLS feature flag (tls-rustls or tls-native-tls).
    /// Must be used together with --tls-key.
    #[arg(long, value_name = "FILE")]
    tls_cert: Option<String>,

    /// Path to TLS private key file (PEM format) for RESP protocol encryption.
    /// Requires a TLS feature flag (tls-rustls or tls-native-tls).
    /// Must be used together with --tls-cert.
    #[arg(long, value_name = "FILE")]
    tls_key: Option<String>,

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
        args.eviction,
        args.failover,
        args.failover_timeout,
        args.replica_peers.map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        }),
        args.tls_cert,
        args.tls_key,
    );

    // Resolve auth tokens from file + CLI
    let auth_tokens = cfg.resolve_tokens();
    let auth_store = if auth_tokens.is_empty() {
        http::auth::AuthStore::new()
    } else {
        http::auth::AuthStore::from_list(auth_tokens)
    };

    let engine = Arc::new(
        match store::shard::EvictionPolicy::from_str_loose(&cfg.server.eviction) {
            Some(policy) => store::engine::KvEngine::with_eviction_policy(
                cfg.server.shard_count,
                cfg.server.max_entries,
                cfg.server.compression_threshold,
                policy,
            ),
            None => {
                eprintln!(
                    "error: invalid eviction policy '{}'. Use: reject, ttl-first, or lru",
                    cfg.server.eviction
                );
                std::process::exit(1);
            }
        },
    );
    let auth = Arc::new(auth_store);
    let share = Arc::new(ShareStore::new());

    // Create readiness state: not ready during snapshot restore
    let ready_state = Arc::new(http::ready::ReadyState::new("restoring_snapshot"));

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

    // Snapshot restore complete (or no snapshot configured): mark as ready
    ready_state.set_ready();

    // Create metrics instance
    let metrics = metrics::create_metrics();

    info!(
        "basalt v{} - memory that moves fast",
        env!("CARGO_PKG_VERSION")
    );
    info!("shards: {}", cfg.server.shard_count);
    info!(
        "compression threshold: {} bytes",
        cfg.server.compression_threshold
    );
    info!("eviction policy: {}", cfg.server.eviction);
    info!("max entries per shard: {}", cfg.server.max_entries);
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

    // Build TLS acceptor if cert/key are configured
    #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
    let tls_acceptor = match resp::tls::build_acceptor(
        cfg.server.tls_cert.as_deref(),
        cfg.server.tls_key.as_deref(),
    ) {
        Ok(acceptor) => {
            if acceptor.is_some() {
                info!(
                    "RESP TLS: enabled (cert: {})",
                    cfg.server.tls_cert.as_deref().unwrap_or("-")
                );
            } else {
                info!("RESP TLS: disabled (no cert/key configured)");
            }
            acceptor
        }
        Err(e) => {
            eprintln!("error: failed to configure TLS: {e}");
            std::process::exit(1);
        }
    };

    #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
    if cfg.server.tls_cert.is_some() || cfg.server.tls_key.is_some() {
        eprintln!(
            "error: TLS cert/key configured but TLS support not compiled in. Enable feature 'tls-rustls' or 'tls-native-tls'."
        );
        std::process::exit(1);
    }
    #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
    info!("RESP TLS: disabled (not compiled in)");

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
    let http_share = share.clone();
    let resp_engine = engine.clone();
    let resp_auth = auth.clone();
    let resp_share = share.clone();
    let http_config = cfg.server.clone();
    let resp_config = cfg.server.clone();
    let http_ready_state = ready_state.clone();

    // Create replication state
    let repl_state = Arc::new(replication::ReplicationState::new_primary(
        engine.clone(),
        cfg.server.wal_size,
    ));
    repl_state.set_failover_config(replication::FailoverConfig::new(
        cfg.server.failover,
        cfg.server.failover_timeout_ms,
        cfg.server.replica_peers.clone(),
    ));
    info!(
        "replication: primary mode, WAL size: {} entries, failover: {}",
        cfg.server.wal_size,
        if cfg.server.failover {
            "enabled"
        } else {
            "disabled"
        },
    );
    if cfg.server.failover {
        info!(
            "failover timeout: {}ms, peers: {:?}",
            cfg.server.failover_timeout_ms, cfg.server.replica_peers
        );
    }

    // Start failover monitor if failover is enabled
    if cfg.server.failover {
        let failover_repl_state = repl_state.clone();
        let failover_ready_state = ready_state.clone();
        let failover_engine = engine.clone();
        let failover_shutdown = shutdown_rx.clone();
        let failover_config = replication::FailoverConfig::new(
            cfg.server.failover,
            cfg.server.failover_timeout_ms,
            cfg.server.replica_peers.clone(),
        );
        info!(
            "failover monitor: starting (timeout: {}ms)",
            cfg.server.failover_timeout_ms
        );
        tokio::spawn(replication::failover_monitor(
            failover_repl_state,
            failover_config,
            failover_ready_state,
            failover_engine,
            failover_shutdown,
        ));
    }

    let http_repl_state = repl_state.clone();
    let http_shutdown_rx = shutdown_rx.clone();
    let http_server = tokio::spawn(async move {
        let app = http::server::app(
            http_engine,
            http_auth,
            http_share,
            http_config.db_path.clone(),
            http_config.snapshot_compression_threshold,
            Some(http_repl_state),
            http_ready_state,
            metrics,
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
            if cfg!(any(feature = "tls-rustls", feature = "tls-native-tls"))
                && (resp_config.tls_cert.is_some() || resp_config.tls_key.is_some())
            {
                eprintln!(
                    "error: TLS is not supported with io_uring RESP backend. Use the tokio backend for TLS."
                );
                std::process::exit(1);
            }
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
                    resp_share,
                    db_path,
                    uring_repl_state,
                ) {
                    eprintln!("io_uring RESP server error: {e}");
                }
            });
            // Keep the tokio task alive until shutdown signal
            resp_shutdown_rx.changed().await.ok();
        } else {
            #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
            let tls_acc = tls_acceptor.clone();
            #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
            let tls_acc = None;
            if let Err(e) = resp::server::run_with_replication_and_tls(
                &resp_config.resp_host,
                resp_config.resp_port,
                resp_engine,
                resp_auth,
                resp_share,
                resp_config.db_path.clone(),
                repl_state.clone(),
                resp_shutdown_rx,
                tls_acc,
            )
            .await
            {
                eprintln!("RESP server error: {e}");
            }
        }

        #[cfg(not(feature = "io-uring"))]
        {
            let rx = resp_shutdown_rx;
            #[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
            let tls_acc = tls_acceptor.clone();
            #[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
            let tls_acc = None;
            if let Err(e) = resp::server::run_with_replication_and_tls(
                &resp_config.resp_host,
                resp_config.resp_port,
                resp_engine,
                resp_auth,
                resp_share,
                resp_config.db_path.clone(),
                repl_state.clone(),
                rx,
                tls_acc,
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
