use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

// Alias para el tipo que devuelve resolve() — un futuro boxeado (ver
// connector.rs para el porqué del Pin<Box<dyn Future>>). Con el alias, el
// trait y sus impls no repiten el tipo completo cada vez.
pub type SecretsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HashMap<String, String>, SecretsError>> + Send + 'a>>;

// SecretsPort — interfaz para resolver credenciales desde un almacén de secrets
// La implementación concreta será Vault, pero el trait permite testear con mocks
pub trait SecretsPort: Send + Sync {
    // Dado un credential_id (ej: "cred-section9-api"),
    // devuelve un HashMap con los secrets resueltos (ej: {"username": "admin", "password": "xxx"})
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_>;
}

#[derive(Debug)]
pub struct SecretsError {
    pub message: String,
}
