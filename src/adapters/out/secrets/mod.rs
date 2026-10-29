// Secrets adapters (driven): resolve credentials. `env` reads from env vars /
// JSON files; `vault` reads KV v2 secrets over HTTP; `cache` wraps a Vault
// with a short resolution TTL; build_chain routes each credential to the
// right one; `mock` is the test double used as the AppBuilder default.
pub mod cache;
pub mod env;
pub mod mock;
pub mod reloadable;
pub mod vault;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::domain::credential::{Credential, CredentialType};
use crate::ports::secrets::{SecretsError, SecretsFuture, SecretsPort};

// The chain: each credential goes to where its values live. One with a
// vault_path reads from the `type: vault` credential it names — each Vault
// its own client, behind its own cache_ttl_seconds (default 60) — or from
// the deprecated config.yaml secrets.vault when it names none. Every other
// one reads env vars or files, uncached: a process's environment cannot
// change under it, and a file is a cheap local read that should pick a
// rotation up at once.
//
// The deprecated config.yaml `secrets.cache_ttl_seconds`, when set, still
// wraps the whole chain in one cache, as it did.
//
// In the library rather than in main because a reload has to build exactly
// the same chain from the new credentials, and two copies of this wiring
// would be two chances for the reloaded one to differ from the booted one.
pub fn build_chain(cfg: &AppConfig) -> Arc<dyn SecretsPort> {
    let credentials = &cfg.credentials;
    // The deprecated outer cache, when set, covers every credential, Vaults
    // included: a Vault's own cache under it would only add its TTL to the
    // time a rotation takes.
    let outer = cfg.secrets_config.cache_ttl_seconds.filter(|ttl| *ttl > 0);
    let vaults = credentials
        .iter()
        .filter(|(_, c)| c.credential_type == CredentialType::Vault)
        .map(|(id, c)| (id.clone(), vault_client(c, credentials, outer.is_none())))
        .collect();
    // The deprecated config.yaml Vault keeps the 60s cache 0.36 gave it by
    // default (secrets.cache_ttl_seconds defaulted to 60 and covered it):
    // without it, a configuration that has not moved yet turns every sync into
    // a Vault request. An explicit cache_ttl_seconds is the outer cache below.
    let legacy = cfg.secrets_config.vault.as_ref().map(|config| {
        let client = Box::new(vault::VaultSecrets::new(
            config.clone(),
            credentials.clone(),
            Box::new(env::EnvSecrets::new(credentials.clone())),
        ));
        match cfg.secrets_config.cache_ttl_seconds {
            None => Arc::new(cache::CachedSecrets::new(
                client,
                std::time::Duration::from_secs(60),
            )) as Arc<dyn SecretsPort>,
            Some(_) => Arc::from(client as Box<dyn SecretsPort>),
        }
    });
    let routed = Routed {
        credentials: credentials.clone(),
        env: env::EnvSecrets::new(credentials.clone()),
        vaults,
        legacy,
    };
    match outer {
        None => Arc::new(routed),
        Some(ttl) => Arc::new(cache::CachedSecrets::new(
            Box::new(routed),
            std::time::Duration::from_secs(ttl),
        )),
    }
}

// One `type: vault` credential as a client, behind its own cache (unless an
// outer one covers it).
fn vault_client(
    vault: &Credential,
    credentials: &HashMap<String, Credential>,
    own_cache: bool,
) -> Arc<dyn SecretsPort> {
    let config = vault::VaultConfig {
        address: vault.address.clone().unwrap_or_default(),
        mount: vault.mount.clone().unwrap_or_else(|| "secret".to_string()),
        token_env: vault.env_vars.get("token").cloned().unwrap_or_default(),
        kubernetes_role: vault.kubernetes_role.clone(),
        jwt_path: vault
            .jwt_path
            .clone()
            .unwrap_or_else(|| vault::DEFAULT_JWT_PATH.to_string()),
        timeout_seconds: vault.timeout_seconds.unwrap_or(10),
    };
    let client = Box::new(vault::VaultSecrets::new(
        config,
        credentials.clone(),
        Box::new(env::EnvSecrets::new(credentials.clone())),
    ));
    match vault.cache_ttl_seconds.unwrap_or(60) {
        _ if !own_cache => Arc::from(client as Box<dyn SecretsPort>),
        0 => Arc::from(client as Box<dyn SecretsPort>),
        ttl => Arc::new(cache::CachedSecrets::new(
            client,
            std::time::Duration::from_secs(ttl),
        )),
    }
}

struct Routed {
    credentials: HashMap<String, Credential>,
    env: env::EnvSecrets,
    vaults: HashMap<String, Arc<dyn SecretsPort>>,
    legacy: Option<Arc<dyn SecretsPort>>,
}

impl SecretsPort for Routed {
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
        let credential_id = credential_id.to_string();
        Box::pin(async move {
            let Some(credential) = self.credentials.get(&credential_id) else {
                return self.env.resolve(&credential_id).await;
            };
            // A Vault connection resolves to nothing a script could use: it
            // "resolves" when it can log in, which for token auth means its
            // variable is set. Asked only by the credentials page's check.
            if credential.credential_type == CredentialType::Vault {
                return match credential.env_vars.get("token") {
                    Some(var) if std::env::var(var).is_err() => Err(SecretsError {
                        message: format!("Env var '{var}' not set"),
                    }),
                    _ => Ok(HashMap::new()),
                };
            }
            if credential.vault_path.is_none() {
                return self.env.resolve(&credential_id).await;
            }
            let vault = match &credential.vault {
                Some(name) => self.vaults.get(name),
                None => self.legacy.as_ref(),
            };
            match vault {
                Some(vault) => vault.resolve(&credential_id).await,
                // Refused at load; reachable only if validation is bypassed.
                None => Err(SecretsError {
                    message: format!("Credential '{credential_id}' names no Vault to read from"),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No config.yaml cache any more: an env credential is read on every
    // resolution, so a changed value is seen at once.
    #[tokio::test]
    async fn an_env_credential_is_read_fresh_every_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cred.yaml"),
            "credentials:\n  - name: cred-a\n    type: token\n    env_vars:\n      token: UNIFIED_API_TEST_FRESH_TOKEN\n",
        )
        .unwrap();
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).unwrap();
        let chain = build_chain(&cfg);

        // SAFETY: a variable no other test reads; set before each resolve.
        unsafe { std::env::set_var("UNIFIED_API_TEST_FRESH_TOKEN", "one") };
        assert_eq!(chain.resolve("cred-a").await.unwrap()["token"], "one");
        unsafe { std::env::set_var("UNIFIED_API_TEST_FRESH_TOKEN", "two") };
        assert_eq!(chain.resolve("cred-a").await.unwrap()["token"], "two");
    }

    // A configuration still on the deprecated config.yaml Vault, with no
    // cache_ttl_seconds, keeps the 60s cache 0.36 gave it by default: two
    // resolutions, one read from Vault.
    #[tokio::test]
    async fn the_deprecated_vault_keeps_its_default_cache() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reads);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                counted.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"data":{"data":{"token":"t"}}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            format!(
                "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  vault:\n    address: {address}\n    token_env: UNIFIED_API_TEST_LEGACY_VAULT_TOKEN\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cred.yaml"),
            "credentials:\n  - name: cred-a\n    type: token\n    vault_path: team/a\n",
        )
        .unwrap();
        // SAFETY: a variable no other test reads.
        unsafe { std::env::set_var("UNIFIED_API_TEST_LEGACY_VAULT_TOKEN", "x") };
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).unwrap();
        let chain = build_chain(&cfg);

        assert_eq!(chain.resolve("cred-a").await.unwrap()["token"], "t");
        assert_eq!(chain.resolve("cred-a").await.unwrap()["token"], "t");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }
}
