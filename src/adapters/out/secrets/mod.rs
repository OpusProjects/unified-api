// Secrets adapters (driven): resolve credentials. They live in
// secrets-rs, shared with unified-gitops: `env` reads from env vars /
// JSON files; `vault` reads KV v2 secrets over HTTP; `cache` wraps a Vault
// with a short resolution TTL; `reloadable` is the swappable chain; `mock` is
// the test double used as the AppBuilder default. Re-exported under their old
// paths, so the rest of the service names them as before.
pub use secrets_rs::{cache, env, mock, reloadable, vault};

use std::sync::Arc;

use crate::config::AppConfig;
use crate::ports::secrets::SecretsPort;

// The chain for this service's configuration: its credentials, and the
// deprecated `secrets:` block of config.yaml. The routing itself — which
// credential reads from env, a file or which Vault — is secrets-rs'
// build_chain, the one place it is decided for both services.
pub fn build_chain(cfg: &AppConfig) -> Arc<dyn SecretsPort> {
    secrets_rs::build_chain(&cfg.credentials, &cfg.secrets_config)
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
