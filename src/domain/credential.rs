use serde::Deserialize;
use std::collections::HashMap;
use std::time::Instant;

// Tipo de credencial — enum en Rust es mucho más potente que en Python,
// pero aquí lo usamos de forma simple: una lista de variantes posibles.
// En Python sería: class CredentialType(Enum): USERNAME_PASSWORD = "username_password"
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    UsernamePassword,
    Token,
    SshKey,
    VaultJwt,
}

// Referencia a una credencial — viene del YAML de configuración.
// No almacena secretos, solo sabe DÓNDE buscarlos en Vault.
#[derive(Debug, Deserialize, Clone)]
pub struct Credential {
    pub name: String,

    // credential_type usa el enum de arriba — solo puede ser uno de los 4 valores
    #[serde(rename = "type")]
    pub credential_type: CredentialType,

    // Ruta en Vault donde están los secretos reales
    pub vault_path: Option<String>,

    // Mapeo de campos: qué key en Vault corresponde a qué campo
    // ej: {"username": "username", "password": "password"}
    #[serde(default)]
    pub vault_keys: HashMap<String, String>,

    // Para tipo VaultJwt
    pub jwt_role: Option<String>,
    pub jwt_auth_path: Option<String>,
}

// Secretos resueltos desde Vault — cacheados en memoria al arrancar.
// Se refrescan solo cuando: fallo de auth, petición manual, o TTL expirado.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub credential_id: String,

    // Los valores reales sacados de Vault (username, password, token, etc.)
    // HashMap porque cada tipo de credencial tiene campos distintos
    pub secrets: HashMap<String, String>,

    // Cuándo se resolvió desde Vault
    pub resolved_at: Instant,
}
