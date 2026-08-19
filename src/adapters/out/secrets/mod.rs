// Secrets adapters (driven): resolve credentials. `env` reads from env vars /
// JSON files; `vault` reads KV v2 secrets over HTTP (falling through to `env`
// for credentials without a vault_path); `cache` wraps any of them with a
// short resolution TTL; `mock` is the test double used as the AppBuilder
// default.
pub mod cache;
pub mod env;
pub mod mock;
pub mod reloadable;
pub mod vault;

use std::sync::Arc;

use crate::config::AppConfig;
use crate::ports::secrets::SecretsPort;

// The chain, innermost out: env/file resolution, optionally fronted by Vault
// (credentials with a vault_path read there, the rest fall through),
// optionally behind the short resolution cache — which stops being a nicety
// and becomes load-bearing the moment Vault turns every resolution into a
// network call.
//
// In the library rather than in main because a reload has to build exactly
// the same chain from the new credentials, and two copies of this wiring
// would be two chances for the reloaded one to differ from the booted one.
pub fn build_chain(cfg: &AppConfig) -> Arc<dyn SecretsPort> {
    let env = env::EnvSecrets::new(cfg.credentials.clone());
    let base: Box<dyn SecretsPort> = match &cfg.secrets_config.vault {
        Some(vault) => Box::new(vault::VaultSecrets::new(
            vault.clone(),
            cfg.credentials.clone(),
            Box::new(env),
        )),
        None => Box::new(env),
    };
    match cfg.secrets_config.cache_ttl_seconds {
        0 => Arc::from(base),
        ttl => Arc::new(cache::CachedSecrets::new(
            base,
            std::time::Duration::from_secs(ttl),
        )),
    }
}
