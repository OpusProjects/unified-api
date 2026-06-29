use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

// SecretsPort — interfaz para resolver credenciales desde un almacén de secrets
// La implementación concreta será Vault, pero el trait permite testear con mocks
pub trait SecretsPort: Send + Sync {
    // Dado un credential_id (ej: "cred-section9-api"),
    // devuelve un HashMap con los secrets resueltos (ej: {"username": "admin", "password": "xxx"})
    fn resolve(
        &self,
        credential_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, String>, SecretsError>> + Send + '_>>;
}

#[derive(Debug)]
pub struct SecretsError {
    pub message: String,
}
