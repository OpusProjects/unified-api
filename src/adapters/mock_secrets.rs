use std::collections::HashMap;

use crate::ports::secrets::{SecretsError, SecretsFuture, SecretsPort};

// Implementation of SecretsPort for tests and local development: no
// secrets store, only what is manually provided. It is the default of
// AppBuilder — production replaces it with EnvSecrets.
//
// Lives in its own file (and not inside env_secrets.rs) to make it
// clear what it is: a test double, not a variant of the real adapter.
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

impl Default for MockSecrets {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretsPort for MockSecrets {
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
        let credential_id = credential_id.to_string();

        Box::pin(async move {
            self.secrets
                .get(&credential_id)
                .cloned()
                .ok_or(SecretsError {
                    message: format!("Mock: credential '{}' not found", credential_id),
                })
        })
    }
}
