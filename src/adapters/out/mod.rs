// Outbound (driven) adapters: the app drives the outside world through these.
// Each implements a driven port from `ports/`.
pub mod cache;
pub mod connectors;
pub mod enrichers;
pub mod output;
pub mod secrets;
