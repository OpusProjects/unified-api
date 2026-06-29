use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    UsernamePassword,
    Token,
    SshKey,
}

// Referencia a una credencial — viene del YAML de configuración.
// No almacena secretos, solo sabe DÓNDE leerlos del entorno.
// La infraestructura (ESO, docker secrets, .env) se encarga de inyectarlos.
#[derive(Debug, Deserialize, Clone)]
pub struct Credential {
    pub name: String,

    #[serde(rename = "type")]
    pub credential_type: CredentialType,

    // Prefijo de env vars — ej: "SECTION9" → lee SECTION9_USERNAME, SECTION9_PASSWORD
    pub env_prefix: Option<String>,

    // Ruta a un fichero JSON con los secrets — ej: "/run/secrets/section9-api.json"
    pub secret_file: Option<String>,

    // Mapeo: nuestro nombre → nombre del campo en env var o JSON
    // ej: {"username": "USERNAME", "password": "PASSWORD"}
    #[serde(default)]
    pub secret_keys: HashMap<String, String>,
}
