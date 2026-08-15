// Secrets adapters (driven): resolve credentials. `env` reads from env vars /
// JSON files; `vault` reads KV v2 secrets over HTTP (falling through to `env`
// for credentials without a vault_path); `cache` wraps any of them with a
// short resolution TTL; `mock` is the test double used as the AppBuilder
// default.
pub mod cache;
pub mod env;
pub mod mock;
pub mod vault;
