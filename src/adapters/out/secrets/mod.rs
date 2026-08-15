// Secrets adapters (driven): resolve credentials. `env` reads from env vars /
// JSON files; `cache` wraps any of them with a short resolution TTL; `mock`
// is the test double used as the AppBuilder default.
pub mod cache;
pub mod env;
pub mod mock;
