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

    // Usamos los sources del YAML real en vez de datos demo
    let app = unified_api::build_app_with_sources(cfg.sources);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap();

    println!("Listening on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}
