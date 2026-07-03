use tracing::{info, error};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Logging estructurado — nivel configurable con RUST_LOG env var
    // RUST_LOG=debug cargo run → muestra debug+info+warn+error
    // RUST_LOG=unified_api=debug → solo debug de nuestro crate
    // Sin RUST_LOG → default info
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info"))
        )
        .init();

    let cfg = match unified_api::config::load_config("config") {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    let auth_enabled = std::env::var("UNIFIED_API_KEY").is_ok();

    info!(
        sources = cfg.sources.len(),
        credentials = cfg.credentials.len(),
        enrichers = cfg.enrichers.len(),
        endpoints = cfg.endpoints.len(),
        auth = auth_enabled,
        "Configuration loaded"
    );

    let (app, state) = unified_api::build_app_production(
        cfg.sources,
        cfg.credentials,
        cfg.enrichers,
        cfg.endpoints,
    );

    unified_api::scheduler::start_sync_tasks(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!(addr = %addr, "Failed to bind: {}", e);
            std::process::exit(1);
        });

    info!(addr = %addr, "Listening");

    // Graceful shutdown — espera SIGTERM o Ctrl+C
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
