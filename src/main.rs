#[tokio::main]
async fn main() {
    let cfg = unified_api::config::load_config("config")
        .expect("Failed to load configuration");

    println!("Loaded {} sources, {} credentials, {} projects, {} endpoints",
        cfg.sources.len(),
        cfg.credentials.len(),
        cfg.projects.len(),
        cfg.endpoints.len(),
    );

    // build_app_with_sources_and_state devuelve (Router, Arc<AppState>)
    // Necesitamos el state para pasárselo al scheduler
    let (app, state) = unified_api::build_app_with_sources_and_state(cfg.sources);

    // Arranca los sync automáticos ANTES de servir HTTP
    // Los tasks corren en background — no bloquean el servidor
    unified_api::scheduler::start_sync_tasks(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap();

    println!("Listening on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}
