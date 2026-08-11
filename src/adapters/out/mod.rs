// Outbound (driven) adapters: the app drives the outside world through these.
// Each implements a driven port from `ports/`.
pub mod cache;
pub mod connectors;
pub mod enrichers;
pub mod git;
pub mod output;
// Not a port implementation: the scrubbed-environment Command builder shared
// by every adapter that spawns a local script.
pub(crate) mod process_env;
pub mod secrets;
