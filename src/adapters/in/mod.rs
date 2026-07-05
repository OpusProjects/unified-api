// Inbound (driving) adapters: the outside world drives the app through these.
// - http/ turns HTTP requests into application calls (axum)
// - scheduler/ turns interval ticks into the same application calls
pub mod http;
pub mod scheduler;
