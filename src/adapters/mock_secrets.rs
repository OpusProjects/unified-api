use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::ports::secrets::{SecretsError, SecretsPort};

// Implementación de SecretsPort para tests y desarrollo local: no hay
// almacén de secrets, solo lo que se le mete a mano. Es el default del
// AppBuilder — producción lo sustituye por EnvSecrets.
//
// Vive en su propio archivo (y no dentro de env_secrets.rs) para que quede
// claro qué es: un doble de pruebas, no una variante del adapter real.
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
