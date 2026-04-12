use clap::Parser;
use std::sync::Arc;
use tracing::info;

pub mod config;
pub mod http;
pub mod resp;
pub mod store;

#[derive(Parser, Debug)]
#[command(name = "basalt", version, about = "Ultra-high performance KV store for AI memory")]
struct Args {
    /// HTTP host to bind to
    #[arg(long, default_value = "127.0.0.1")]
    http_host: String,

    /// HTTP port for the REST API
    #[arg(long, default_value_t = 7380)]
    http_port: u16,

    /// RESP host to bind to
    #[arg(long, default_value = "127.0.0.1")]
    resp_host: String,

    /// RESP port (Redis-compatible protocol)
    #[arg(long, default_value_t = 6380)]
    resp_port: u16,

    /// Number of shards (rounded to next power of 2)
    #[arg(long, default_value_t = 64)]
    shards: usize,
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
    let config = config::Config {
        http_host: args.http_host,
        http_port: args.http_port,
        resp_host: args.resp_host,
        resp_port: args.resp_port,
        shard_count: args.shards,
    };

    let engine = Arc::new(store::engine::KvEngine::new(config.shard_count));

    info!("basalt v{} — memory that moves fast", env!("CARGO_PKG_VERSION"));
    info!("shards: {}", config.shard_count);
    info!("HTTP:  {}:{}", config.http_host, config.http_port);
    info!("RESP:  {}:{}", config.resp_host, config.resp_port);

    // Start both servers concurrently
    let http_engine = engine.clone();
    let resp_engine = engine.clone();
    let http_config = config.clone();
    let resp_config = config.clone();

    let http_server = tokio::spawn(async move {
        let app = http::server::app(http_engine);
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
