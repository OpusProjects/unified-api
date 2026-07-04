// The adapters: everything that touches the outside world, in both directions.
// - Driving (incoming requests): http/ (axum) and scheduler (timers)
// - Driven (we go out): cache, connectors, secrets, output
pub mod env_secrets;
pub mod http;
pub mod memory_cache;
pub mod mock_secrets;
pub mod process_connector;
pub mod process_enricher;
pub mod process_output;
pub mod scheduler;
pub mod ssh_connector;
