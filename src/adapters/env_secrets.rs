use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::domain::credential::Credential;
use crate::ports::secrets::{SecretsError, SecretsPort};

// Lee secrets de env vars o ficheros JSON — sin dependencias externas.
// La infraestructura (ESO → k8s Secret → envFrom, docker secrets, .env) inyecta los valores.
pub struct EnvSecrets {
    credentials: HashMap<String, Credential>,
}

impl EnvSecrets {
    pub fn new(credentials: HashMap<String, Credential>) -> Self {
        Self { credentials }
    }
}

impl SecretsPort for EnvSecrets {
    fn resolve(
        &self,
        credential_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, String>, SecretsError>> + Send + '_>>
    {
        let credential_id = credential_id.to_string();

        Box::pin(async move {
            let credential = self.credentials.get(&credential_id).ok_or(SecretsError {
                message: format!("Credential '{}' not found in config", credential_id),
            })?;

            let mut secrets = HashMap::new();

            // Resuelve secret_keys desde env vars o fichero JSON
            if !credential.secret_keys.is_empty() {
                let resolved = if let Some(ref prefix) = credential.env_prefix {
                    resolve_from_env(prefix, &credential.secret_keys)
                } else if let Some(ref path) = credential.secret_file {
                    resolve_from_file(path, &credential.secret_keys)
                } else {
                    Err(SecretsError {
                        message: format!(
                            "Credential '{}' has secret_keys but no env_prefix or secret_file",
                            credential_id
                        ),
                    })
                }?;
                secrets.extend(resolved);
            }

            // Añade file_keys como {key}_path → path del fichero
            for (key, path) in &credential.file_keys {
                let path_key = format!("{}_path", key);
                secrets.insert(path_key, path.clone());
            }

            if secrets.is_empty() {
                return Err(SecretsError {
                    message: format!(
                        "Credential '{}' has no secret_keys or file_keys",
                        credential_id
                    ),
                });
            }

            Ok(secrets)
        })
    }
}

// Lee PREFIJO_CAMPO de env vars
// ej: prefix="SECTION9", secret_keys={"username": "USERNAME"}
//   → lee env var SECTION9_USERNAME
fn resolve_from_env(
    prefix: &str,
    secret_keys: &HashMap<String, String>,
) -> Result<HashMap<String, String>, SecretsError> {
    let mut secrets = HashMap::new();

    for (our_key, env_suffix) in secret_keys {
        let env_var = format!("{}_{}", prefix, env_suffix);
        let value = std::env::var(&env_var).map_err(|_| SecretsError {
            message: format!("Env var '{}' not set", env_var),
        })?;
        secrets.insert(our_key.clone(), value);
    }

    Ok(secrets)
}

// Lee un fichero JSON y extrae los campos según secret_keys
fn resolve_from_file(
    path: &str,
    secret_keys: &HashMap<String, String>,
) -> Result<HashMap<String, String>, SecretsError> {
    let content = std::fs::read_to_string(path).map_err(|e| SecretsError {
        message: format!("Failed to read secret file '{}': {}", path, e),
    })?;

    let json: serde_json::Value = serde_json::from_str(&content).map_err(|e| SecretsError {
        message: format!("Failed to parse secret file '{}': {}", path, e),
    })?;

    let mut secrets = HashMap::new();

    for (our_key, json_key) in secret_keys {
        let value = json
            .get(json_key)
            .and_then(|v| v.as_str())
            .ok_or(SecretsError {
                message: format!("Key '{}' not found in secret file '{}'", json_key, path),
            })?;
        secrets.insert(our_key.clone(), value.to_string());
    }

    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::credential::CredentialType;

    #[tokio::test]
    async fn resolve_from_env_vars() {
        unsafe {
            std::env::set_var("TEST_CRED_USERNAME", "admin");
            std::env::set_var("TEST_CRED_PASSWORD", "secret123");
        }

        let mut credentials = HashMap::new();
        credentials.insert("cred-test".to_string(), Credential {
            name: "Test".to_string(),
            credential_type: CredentialType::UsernamePassword,
            env_prefix: Some("TEST_CRED".to_string()),
            secret_file: None,
            secret_keys: [
                ("username".to_string(), "USERNAME".to_string()),
                ("password".to_string(), "PASSWORD".to_string()),
            ].into_iter().collect(),
            file_keys: HashMap::new(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-test").await.unwrap();

        assert_eq!(result["username"], "admin");
        assert_eq!(result["password"], "secret123");

        unsafe {
            std::env::remove_var("TEST_CRED_USERNAME");
            std::env::remove_var("TEST_CRED_PASSWORD");
        }
    }

    #[tokio::test]
    async fn resolve_from_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("creds.json");
        std::fs::write(&secret_path, r#"{"USER": "dbadmin", "PASS": "dbpass"}"#).unwrap();

        let mut credentials = HashMap::new();
        credentials.insert("cred-db".to_string(), Credential {
            name: "DB".to_string(),
            credential_type: CredentialType::UsernamePassword,
            env_prefix: None,
            secret_file: Some(secret_path.to_str().unwrap().to_string()),
            secret_keys: [
                ("username".to_string(), "USER".to_string()),
                ("password".to_string(), "PASS".to_string()),
            ].into_iter().collect(),
            file_keys: HashMap::new(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-db").await.unwrap();

        assert_eq!(result["username"], "dbadmin");
        assert_eq!(result["password"], "dbpass");
    }

    #[tokio::test]
    async fn resolve_file_keys() {
        let mut credentials = HashMap::new();
        credentials.insert("cred-ssh".to_string(), Credential {
            name: "SSH".to_string(),
            credential_type: CredentialType::SshKey,
            env_prefix: Some("SSH_TEST".to_string()),
            secret_file: None,
            secret_keys: HashMap::new(),
            file_keys: [
                ("ssh_key".to_string(), "/run/secrets/id_rsa".to_string()),
            ].into_iter().collect(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-ssh").await.unwrap();

        assert_eq!(result["ssh_key_path"], "/run/secrets/id_rsa");
    }

    #[tokio::test]
    async fn resolve_mixed_secret_keys_and_file_keys() {
        unsafe { std::env::set_var("MIX_TEST_USERNAME", "sshuser"); }

        let mut credentials = HashMap::new();
        credentials.insert("cred-mix".to_string(), Credential {
            name: "Mixed".to_string(),
            credential_type: CredentialType::SshKey,
            env_prefix: Some("MIX_TEST".to_string()),
            secret_file: None,
            secret_keys: [
                ("username".to_string(), "USERNAME".to_string()),
            ].into_iter().collect(),
            file_keys: [
                ("ssh_key".to_string(), "/run/secrets/id_rsa".to_string()),
                ("ca_cert".to_string(), "/run/secrets/ca.pem".to_string()),
            ].into_iter().collect(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-mix").await.unwrap();

        assert_eq!(result["username"], "sshuser");
        assert_eq!(result["ssh_key_path"], "/run/secrets/id_rsa");
        assert_eq!(result["ca_cert_path"], "/run/secrets/ca.pem");
        assert_eq!(result.len(), 3);

        unsafe { std::env::remove_var("MIX_TEST_USERNAME"); }
    }

    #[tokio::test]
    async fn resolve_only_file_keys_no_secret_keys() {
        let mut credentials = HashMap::new();
        credentials.insert("cred-cert".to_string(), Credential {
            name: "Cert Only".to_string(),
            credential_type: CredentialType::SshKey,
            env_prefix: None,
            secret_file: None,
            secret_keys: HashMap::new(),
            file_keys: [
                ("client_cert".to_string(), "/run/secrets/client.pem".to_string()),
            ].into_iter().collect(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-cert").await.unwrap();

        assert_eq!(result["client_cert_path"], "/run/secrets/client.pem");
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn resolve_empty_credential_fails() {
        let mut credentials = HashMap::new();
        credentials.insert("cred-empty".to_string(), Credential {
            name: "Empty".to_string(),
            credential_type: CredentialType::Token,
            env_prefix: None,
            secret_file: None,
            secret_keys: HashMap::new(),
            file_keys: HashMap::new(),
        });

        let secrets = EnvSecrets::new(credentials);
        let result = secrets.resolve("cred-empty").await;

        assert!(result.is_err());
    }
}
