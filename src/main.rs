use axum::{routing::get, Router};
use std::sync::Arc;

mod adapters;
mod api;
mod config;
mod domain;
mod ports;

use adapters::memory_cache::MemoryCache;
use domain::cache_entry::CacheEntry;
use domain::dataset::Dataset;

// AppState contiene todo lo que los handlers HTTP necesitan acceder.
// Arc = "Atomic Reference Counter" — un puntero inteligente que permite
// compartir datos entre threads de forma segura. Varios handlers pueden
// leer el mismo cache al mismo tiempo sin problemas.
//
// `dyn CachePort` = "cualquier tipo que implemente CachePort"
// Hoy es MemoryCache, mañana podría ser Redis, sin cambiar los handlers.
pub struct AppState {
    pub cache: Arc<dyn ports::cache::CachePort>,
}

#[tokio::main]
async fn main() {
    let cfg = config::load_config("config")
        .expect("Failed to load configuration");

    println!("Loaded {} sources, {} credentials, {} projects, {} endpoints",
        cfg.sources.len(),
        cfg.credentials.len(),
        cfg.projects.len(),
        cfg.endpoints.len(),
    );

    // Creamos el estado compartido: el cache en memoria
    // Arc::new() envuelve el MemoryCache para que sea compartible entre threads
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
    });

    // .with_state() pasa el estado a todos los handlers
    // --- Datos de prueba para ver el flujo completo ---
    // En producción esto lo haría el sync engine al ejecutar un connector
    let demo_dataset: Dataset = serde_json::from_str(r#"{
        "hostvars": {
            "host001.dc06.pqe": {
                "ansible_host": "10.1.2.3",
                "os": "OracleLinux",
                "datacenter": "DC06"
            },
            "host002.dc06.pqe": {
                "ansible_host": "10.1.2.4",
                "os": "OracleLinux",
                "datacenter": "DC06"
            }
        },
        "groups": {
            "dc06": {
                "hosts": ["host001.dc06.pqe", "host002.dc06.pqe"],
                "vars": {"ntp_server": "ntp.dc06.pqe"}
            },
            "oraclelinux": {
                "hosts": ["host001.dc06.pqe", "host002.dc06.pqe"]
            }
        }
    }"#).expect("Failed to parse demo dataset");

    // Guardamos en cache con TTL de 3600 segundos
    state.cache.set("src-device42", CacheEntry::new(demo_dataset, 3600));
    println!("Loaded demo dataset with 2 hosts into cache");

    let app = Router::new()
        .route("/healthz", get(api::health::healthz))
        .route("/readyz", get(api::health::readyz))
        .route("/api/v1/sources", get(api::sources::list_cached_sources))
        .route("/api/v1/sources/{id}/dataset", get(api::sources::get_source_dataset))
        .with_state(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap();

    println!("Listening on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}
