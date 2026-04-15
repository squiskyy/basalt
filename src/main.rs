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

    /// Number of entries to sample for approximate LRU eviction (overrides config file, default: 20)
    #[arg(long, value_name = "N")]
    lru_sample_size: Option<usize>,

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

    /// LLM provider: "openai", "anthropic", or "custom" (overrides config file, empty = disabled)
    #[arg(long, value_name = "PROVIDER")]
    llm_provider: Option<String>,

    /// LLM API key (overrides config file)
    #[arg(long, value_name = "KEY")]
    llm_api_key: Option<String>,

    /// LLM model to use (overrides config file)
    #[arg(long, value_name = "MODEL")]
    llm_model: Option<String>,

    /// LLM base URL override for OpenAI-compatible endpoints (overrides config file)
    #[arg(long, value_name = "URL")]
    llm_base_url: Option<String>,
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
        args.lru_sample_size,
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
        args.llm_provider,
        args.llm_api_key,
        args.llm_model,
        args.llm_base_url,
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
            Some(policy) => store::engine::KvEngine::with_eviction_policy_and_sample_size(
                cfg.server.shard_count,
                cfg.server.max_entries,
                cfg.server.compression_threshold,
                policy,
                cfg.server.lru_sample_size,
                Arc::new(store::ConsolidationManager::disabled()),
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

    // Construct LLM client from config
    let llm_client = if cfg.llm.is_enabled() {
        match cfg.llm.provider.as_str() {
            "openai" | "custom" => {
                let base_url = if cfg.llm.base_url.is_empty() {
                    "https://api.openai.com/v1".to_string()
                } else {
                    cfg.llm.base_url.clone()
                };
                if cfg.llm.provider == "custom" && base_url == "https://api.openai.com/v1" {
                    eprintln!("error: LLM provider is 'custom' but no base_url configured");
                    std::process::exit(1);
                }
                let openai_config = basalt::llm::OpenAiConfig {
                    api_key: cfg.llm.api_key.clone(),
                    model: if cfg.llm.model.is_empty() {
                        "gpt-4o-mini".into()
                    } else {
                        cfg.llm.model.clone()
                    },
                    base_url,
                    timeout_ms: cfg.llm.timeout_ms,
                    default_max_tokens: cfg.llm.default_max_tokens,
                    default_temperature: cfg.llm.default_temperature,
                };
                match basalt::llm::OpenAiProvider::new(openai_config) {
                    Ok(provider) => {
                        info!(
                            "LLM: enabled (provider={}, model={}, base_url={})",
                            cfg.llm.provider, cfg.llm.model, cfg.llm.base_url
                        );
                        basalt::llm::LlmClient::with_provider(Arc::new(provider))
                    }
                    Err(e) => {
                        eprintln!("error: failed to create LLM client: {e}");
                        std::process::exit(1);
                    }
                }
            }
            "anthropic" => {
                let anthropic_config = basalt::llm::AnthropicConfig {
                    api_key: cfg.llm.api_key.clone(),
                    model: if cfg.llm.model.is_empty() {
                        "claude-sonnet-4-20250514".into()
                    } else {
                        cfg.llm.model.clone()
                    },
                    base_url: if cfg.llm.base_url.is_empty() {
                        "https://api.anthropic.com".to_string()
                    } else {
                        cfg.llm.base_url.clone()
                    },
                    api_version: cfg.llm.api_version.clone(),
                    timeout_ms: cfg.llm.timeout_ms,
                    default_max_tokens: cfg.llm.default_max_tokens,
                    default_temperature: cfg.llm.default_temperature,
                };
                match basalt::llm::AnthropicProvider::new(anthropic_config) {
                    Ok(provider) => {
                        info!("LLM: enabled (provider=anthropic, model={})", cfg.llm.model);
                        basalt::llm::LlmClient::with_provider(Arc::new(provider))
                    }
                    Err(e) => {
                        eprintln!("error: failed to create LLM client: {e}");
                        std::process::exit(1);
                    }
                }
            }
            other => {
                eprintln!(
                    "error: unknown LLM provider: '{other}' (use 'openai', 'anthropic', or 'custom')"
                );
                std::process::exit(1);
            }
        }
    } else {
        info!("LLM: disabled (no provider configured)");
        basalt::llm::LlmClient::disabled()
    };
    let _llm_client = Arc::new(llm_client);

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
    if cfg.server.eviction == "lru" {
        info!("lru sample size: {}", cfg.server.lru_sample_size);
    }
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

    // Apply default decay lambda from config
    {
        let default_config = engine.decay_config().defaults();
        let mut defaults = default_config.clone();
        defaults.lambda = cfg.server.decay_default_lambda;
        engine.decay_config().set("_default", defaults);
    }

    // Start decay reap loop if configured
    if cfg.server.decay_reap_interval_ms > 0 {
        let decay_engine = engine.clone();
        let decay_interval = cfg.server.decay_reap_interval_ms;
        let mut decay_shutdown = shutdown_rx.clone();
        info!("decay reap enabled: every {}ms", decay_interval);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(decay_interval)) => {
                        let reaped = decay_engine.reap_all_low_relevance().await;
                        if reaped > 0 {
                            tracing::debug!("decay reap: removed {reaped} low-relevance entries");
                        }
                    }
                    _ = decay_shutdown.changed() => {
                        tracing::info!("decay reap shutting down");
                        break;
                    }
                }
            }
        });
    } else {
        info!("decay reap: disabled (interval = 0)");
    }

    // Start trigger sweep loop if configured
    if cfg.server.trigger_sweep_interval_ms > 0 {
        let sweep_engine = engine.clone();
        let sweep_interval = cfg.server.trigger_sweep_interval_ms;
        let mut sweep_shutdown = shutdown_rx.clone();
        info!("trigger sweep: enabled, every {}ms", sweep_interval);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(sweep_interval)) => {
                        let triggers = sweep_engine.trigger_manager().list();
                        let now_ms = basalt::time::now_ms();
                        for info in triggers {
                            if !info.enabled {
                                continue;
                            }
                            if let Some(entries) = sweep_engine.check_trigger_condition(&info.condition, now_ms)
                                && let Some(ctx) = sweep_engine.trigger_manager().try_fire(&info.id, entries, now_ms)
                            {
                                let action_config = sweep_engine.trigger_manager().get_action_config(&info.id);
                                let action = sweep_engine.trigger_manager().get_action(&info.id);
                                if let Some(config) = action_config {
                                    let id = info.id.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = store::trigger::execute_webhook(&config, ctx).await {
                                            tracing::warn!("trigger webhook failed: id={}, error={}", id, e);
                                        }
                                    });
                                } else if let Some(action_fn) = action {
                                    tokio::spawn(async move {
                                        action_fn(ctx).await;
                                    });
                                }
                            }
                        }
                    }
                    _ = sweep_shutdown.changed() => {
                        tracing::info!("trigger sweep shutting down");
                        break;
                    }
                }
            }
        });
    } else {
        info!("trigger sweep: disabled (interval = 0)");
    }

    // Start consolidation sweep loop if configured
    if cfg.server.consolidation_interval_ms > 0 {
        let con_engine = engine.clone();
        let con_interval = cfg.server.consolidation_interval_ms;
        let mut con_shutdown = shutdown_rx.clone();
        info!("consolidation sweep: enabled, every {}ms", con_interval);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(con_interval)) => {
                        let namespaces = con_engine.active_namespaces();
                        let rules = con_engine.consolidation_manager().rules();
                        for namespace in &namespaces {
                            let result = store::consolidation::run_consolidation_with_llm(
                                &con_engine,
                                namespace,
                                &rules,
                                None,
                            ).await;
                            if result.promoted > 0 || result.compressed > 0 {
                                info!(
                                    "consolidation sweep: namespace={}, promoted={}, compressed={}, skipped={}",
                                    namespace, result.promoted, result.compressed, result.skipped,
                                );
                            }
                            let mut meta = con_engine.consolidation_manager().get_meta(namespace)
                                .unwrap_or_default();
                            meta.last_run_ms = basalt::time::now_ms();
                            meta.total_promoted += result.promoted as u64;
                            meta.total_compressed += result.compressed as u64;
                            con_engine.consolidation_manager().update_meta(namespace, meta);
                            tokio::task::yield_now().await;
                        }
                    }
                    _ = con_shutdown.changed() => {
                        tracing::info!("consolidation sweep shutting down");
                        break;
                    }
                }
            }
        });
    } else {
        info!("consolidation sweep: disabled (interval = 0)");
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
            http_config.consolidation_interval_ms,
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
