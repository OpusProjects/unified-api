// El adapter HTTP (driving adapter): handlers de axum, auth, rutas y la
// spec OpenAPI. Cada archivo agrupa los handlers de un recurso.
pub mod auth;
pub mod endpoints;
pub mod enrichers;
pub mod health;
pub mod hosts;
pub mod openapi;
pub mod routes;
pub mod sources;
pub mod sync;
