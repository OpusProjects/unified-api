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

    let (app, state) = unified_api::build_app_production(cfg.sources, cfg.credentials);

    unified_api::scheduler::start_sync_tasks(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap();

    println!("Listening on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}
