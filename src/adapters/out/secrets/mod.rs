// Secrets adapters (driven): resolve credentials. `env` reads from env vars /
// JSON files; `mock` is the test double used as the AppBuilder default.
pub mod env;
pub mod mock;
