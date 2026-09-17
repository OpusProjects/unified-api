// The HTTP adapter (driving adapter): axum handlers, auth, routes, and
// OpenAPI spec. Each file groups handlers for a single resource.
pub mod audit;
pub mod auth;
pub mod cache;
pub mod config;
pub mod detail;
pub mod endpoints;
pub mod enrichers;
pub mod error;
pub mod health;
pub mod hosts;
pub mod html;
pub mod index;
pub mod login;
pub mod metrics;
pub mod openapi;
pub mod projects;
pub mod routes;
pub mod scope;
pub mod session;
pub mod sources;
pub mod spec;
pub mod sync;
pub mod views;
