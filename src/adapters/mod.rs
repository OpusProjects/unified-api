// The adapters: everything that touches the outside world, split by direction.
// - `in` (driving): the outside world drives us — http/ (axum) and scheduler/ (timers)
// - `out` (driven): we drive the outside world — cache, connectors, enrichers, output, secrets
//
// `in` is a Rust keyword, so the module is spelled `r#in` (the folder is `in/`).
pub mod r#in;
pub mod out;
