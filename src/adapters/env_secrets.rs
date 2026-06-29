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

            // Intenta env_prefix primero, luego secret_file
            if let Some(ref prefix) = credential.env_prefix {
                resolve_from_env(prefix, &credential.secret_keys)
            } else if let Some(ref path) = credential.secret_file {
                resolve_from_file(path, &credential.secret_keys)
            } else {
                Err(SecretsError {
                    message: format!(
                        "Credential '{}' has no env_prefix or secret_file",
                        credential_id
                    ),
                })
            }
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

// Mock para tests
pub struct MockSecrets {
    secrets: HashMap<String, HashMap<String, String>>,
}

impl MockSecrets {
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }
}

impl SecretsPort for MockSecrets {
    fn resolve(
        &self,
        credential_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, String>, SecretsError>> + Send + '_>>
    {
        let credential_id = credential_id.to_string();

        Box::pin(async move {
            self.secrets.get(&credential_id).cloned().ok_or(SecretsError {
                message: format!("Mock: credential '{}' not found", credential_id),
            })
        })
    }
}
