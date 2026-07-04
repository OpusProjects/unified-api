use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use unified_api::adapters::env_secrets::EnvSecrets;

#[tokio::main]
async fn main() {
    // Structured logging — level configurable with RUST_LOG env var
    // RUST_LOG=debug cargo run → shows debug+info+warn+error
    // RUST_LOG=unified_api=debug → only debug from our crate
    // Without RUST_LOG → default is info
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_dir = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "config".to_string());
    let cfg = match unified_api::config::load_config(&config_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // The API key is read here, at the boundary: the rest of the app receives it
    // as a parameter and does not touch environment variables
    let api_key = std::env::var("UNIFIED_API_KEY").ok();

    info!(
        sources = cfg.sources.len(),
        credentials = cfg.credentials.len(),
        enrichers = cfg.enrichers.len(),
        endpoints = cfg.endpoints.len(),
        projects = cfg.projects.len(),
        auth = api_key.is_some(),
        "Configuration loaded"
    );

    let (app, state) = unified_api::AppBuilder::new()
        .sources(cfg.sources)
        .enrichers(cfg.enrichers)
        .endpoints(cfg.endpoints)
        .secrets(std::sync::Arc::new(EnvSecrets::new(cfg.credentials)))
        .api_key(api_key)
        .build_with_state();

    unified_api::adapters::scheduler::start_sync_tasks(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!(addr = %addr, "Failed to bind: {}", e);
            std::process::exit(1);
        });

    info!(addr = %addr, "Listening");

    // Graceful shutdown — waits for SIGTERM or Ctrl+C
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            error!("Server error: {}", e);
            std::process::exit(1);
        });

    info!("Shutdown complete");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down"),
        _ = terminate => info!("Received SIGTERM, shutting down"),
    }
}
