// Los adapters: TODO lo que toca el mundo exterior, en ambas direcciones.
// - Driving (entran peticiones): http/ (axum) y scheduler (timers)
// - Driven (salimos nosotros): cache, connectors, secrets, output
pub mod env_secrets;
pub mod http;
pub mod memory_cache;
pub mod mock_secrets;
pub mod process_connector;
pub mod process_enricher;
pub mod process_output;
pub mod scheduler;
pub mod ssh_connector;
