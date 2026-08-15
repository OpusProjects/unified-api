use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::domain::credential::Credential;
use crate::ports::secrets::{SecretsError, SecretsFuture, SecretsPort};

// Native HashiCorp Vault resolution (KV v2 over HTTP) — the adapter the docs
// promised as roadmap since the SecretsPort existed.
//
// Scope, deliberately narrow: read-only KV v2 (`GET /v1/<mount>/data/<path>`),
// with two auth methods — a token from an env var, or Kubernetes auth
// (exchange the pod's service-account JWT for a client token). A credential
// that carries `vault_path` resolves here; one that does not falls through to
// the inner adapter (EnvSecrets), so a deployment can move credentials to
// Vault one at a time.
//
// Failures surface exactly like any other resolution failure: the sync fails,
// names the credential, and lands in `sync_health` — no new machinery.

// Config shape of the `secrets.vault:` block in config.yaml. Defined next to
// the adapter it configures and embedded by config::SecretsConfig; strict
// like every config struct (see the policy comment in config.rs).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    // e.g. "https://vault.example.com:8200" (scheme required)
    pub address: String,

    // The KV v2 mount `vault_path` values live under (default "secret")
    #[serde(default = "default_mount")]
    pub mount: String,

    // Env var holding a Vault token (default VAULT_TOKEN). Read on every
    // login-less resolution, so rotating the token needs no restart. Used
    // only when kubernetes_role is not set.
    #[serde(default = "default_token_env")]
    pub token_env: String,

    // Kubernetes auth: when set, the pod's service-account JWT is exchanged
    // at auth/kubernetes/login for a client token, cached until shortly
    // before its lease expires.
    #[serde(default)]
    pub kubernetes_role: Option<String>,

    // Where the service-account JWT lives (the k8s default path)
    #[serde(default = "default_jwt_path")]
    pub jwt_path: String,

    // Bound on every Vault request, same convention as everything else that
    // talks to the outside world
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_mount() -> String {
    "secret".to_string()
}

fn default_token_env() -> String {
    "VAULT_TOKEN".to_string()
}

fn default_jwt_path() -> String {
    "/var/run/secrets/kubernetes.io/serviceaccount/token".to_string()
}

fn default_timeout() -> u64 {
    10
}

// A Kubernetes-auth client token and the moment we stop trusting it.
struct LoginToken {
    token: String,
    expires_at: Instant,
}

pub struct VaultSecrets {
    config: VaultConfig,
    credentials: HashMap<String, Credential>,
    // Credentials WITHOUT a vault_path resolve here (EnvSecrets in
    // production), so Vault adoption is per credential, not all-or-nothing
    fallback: Box<dyn SecretsPort>,
    http: reqwest::Client,
    login: Mutex<Option<LoginToken>>,
}

impl VaultSecrets {
    pub fn new(
        config: VaultConfig,
        credentials: HashMap<String, Credential>,
        fallback: Box<dyn SecretsPort>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .expect("reqwest client with a timeout always builds");
        Self {
            config,
            credentials,
            fallback,
            http,
            login: Mutex::new(None),
        }
    }

    fn base(&self) -> &str {
        self.config.address.trim_end_matches('/')
    }

    // The token to present: the env var, or a cached/renewed Kubernetes login.
    async fn auth_token(&self) -> Result<String, SecretsError> {
        let Some(role) = &self.config.kubernetes_role else {
            return std::env::var(&self.config.token_env).map_err(|_| SecretsError {
                message: format!(
                    "Vault token env var '{}' is not set (secrets.vault.token_env)",
                    self.config.token_env
                ),
            });
        };

        if let Some(cached) = self
            .login
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|login| Instant::now() < login.expires_at)
        {
            return Ok(cached.token.clone());
        }

        let jwt = tokio::fs::read_to_string(&self.config.jwt_path)
            .await
            .map_err(|e| SecretsError {
                message: format!("read service-account JWT '{}': {}", self.config.jwt_path, e),
            })?;

        let url = format!("{}/v1/auth/kubernetes/login", self.base());
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "role": role, "jwt": jwt.trim() }))
            .send()
            .await
            .map_err(|e| SecretsError {
                message: format!("Vault login: {}", e),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(SecretsError {
                message: format!("Vault login failed ({})", status),
            });
        }

        let body: serde_json::Value = response.json().await.map_err(|e| SecretsError {
            message: format!("Vault login: unparseable response: {}", e),
        })?;
        let token = body
            .pointer("/auth/client_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SecretsError {
                message: "Vault login: response carries no auth.client_token".to_string(),
            })?
            .to_string();
        // Renew at 80% of the lease: better a login too many than a request
        // sent with a token that expires mid-flight.
        let lease: u64 = body
            .pointer("/auth/lease_duration")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        let expires_at = Instant::now() + Duration::from_secs(lease.saturating_mul(4) / 5);

        *self
            .login
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LoginToken {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }
}

// The `secret_keys` mapping, applied to a Vault secret's data exactly as the
// env adapter applies it to env suffixes: our name → field in the secret.
// Empty = take every field verbatim, since unlike an environment there is
// nothing else in the secret to accidentally sweep up. Non-string values
// (ports, flags) are handed to scripts in their JSON rendering.
fn map_secret_data(
    credential_id: &str,
    secret_keys: &HashMap<String, String>,
    data: &serde_json::Map<String, serde_json::Value>,
) -> Result<HashMap<String, String>, SecretsError> {
    let render = |value: &serde_json::Value| match value.as_str() {
        Some(s) => s.to_string(),
        None => value.to_string(),
    };

    if secret_keys.is_empty() {
        return Ok(data
            .iter()
            .map(|(key, value)| (key.clone(), render(value)))
            .collect());
    }

    secret_keys
        .iter()
        .map(|(ours, theirs)| {
            data.get(theirs)
                .map(|value| (ours.clone(), render(value)))
                .ok_or_else(|| SecretsError {
                    message: format!(
                        "credential '{}': field '{}' is not present in the Vault secret",
                        credential_id, theirs
                    ),
                })
        })
        .collect()
}

impl SecretsPort for VaultSecrets {
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
        let credential_id = credential_id.to_string();

        Box::pin(async move {
            let credential = self.credentials.get(&credential_id).ok_or(SecretsError {
                message: format!("Credential '{}' not found in config", credential_id),
            })?;

            let Some(vault_path) = &credential.vault_path else {
                return self.fallback.resolve(&credential_id).await;
            };

            let token = self.auth_token().await?;
            let url = format!(
                "{}/v1/{}/data/{}",
                self.base(),
                self.config.mount,
                vault_path
            );
            let response = self
                .http
                .get(&url)
                .header("X-Vault-Token", token)
                .send()
                .await
                .map_err(|e| SecretsError {
                    message: format!("Vault read '{}': {}", vault_path, e),
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(SecretsError {
                    message: format!(
                        "Vault read '{}/{}' failed ({})",
                        self.config.mount, vault_path, status
                    ),
                });
            }

            let body: serde_json::Value = response.json().await.map_err(|e| SecretsError {
                message: format!("Vault read '{}': unparseable response: {}", vault_path, e),
            })?;
            // KV v2 nests the fields under data.data (the outer data carries
            // version metadata). A v1 mount answers without the nesting and
            // fails here on purpose — this adapter speaks v2 only, and half
            // reading a v1 secret would be worse than saying so.
            let data = body
                .pointer("/data/data")
                .and_then(|v| v.as_object())
                .ok_or_else(|| SecretsError {
                    message: format!(
                        "Vault secret '{}/{}' is not KV v2 shaped (no data.data) — \
                         is the mount a KV version 2 engine?",
                        self.config.mount, vault_path
                    ),
                })?;

            let mut secrets = map_secret_data(&credential_id, &credential.secret_keys, data)?;

            // file_keys keep meaning "a path the script consumes directly",
            // exactly as with the env adapter
            for (key, path) in &credential.file_keys {
                secrets.insert(format!("{}_path", key), path.clone());
            }

            Ok(secrets)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().expect("object fixture").clone()
    }

    #[test]
    fn secret_keys_map_our_names_to_vault_fields() {
        let keys: HashMap<String, String> = [
            ("username".to_string(), "user".to_string()),
            ("password".to_string(), "pass".to_string()),
        ]
        .into_iter()
        .collect();
        let mapped = map_secret_data(
            "cred-a",
            &keys,
            &data(serde_json::json!({"user": "motoko", "pass": "s3cret", "extra": "ignored"})),
        )
        .expect("maps");

        assert_eq!(mapped["username"], "motoko");
        assert_eq!(mapped["password"], "s3cret");
        assert!(
            !mapped.contains_key("extra"),
            "unmapped fields stay out when a mapping is declared"
        );
    }

    #[test]
    fn empty_secret_keys_take_every_field() {
        let mapped = map_secret_data(
            "cred-a",
            &HashMap::new(),
            &data(serde_json::json!({"token": "abc", "port": 8200})),
        )
        .expect("maps");

        assert_eq!(mapped["token"], "abc");
        // Non-string values arrive in their JSON rendering
        assert_eq!(mapped["port"], "8200");
    }

    #[test]
    fn a_missing_field_names_itself() {
        let keys: HashMap<String, String> = [("password".to_string(), "pass".to_string())]
            .into_iter()
            .collect();
        let err = map_secret_data("cred-a", &keys, &data(serde_json::json!({"user": "m"})))
            .expect_err("field is missing");

        assert!(err.message.contains("'pass'"), "error was: {}", err.message);
        assert!(err.message.contains("cred-a"));
    }
}
