use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::domain::credential::Credential;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::project::GitProject;
use crate::domain::source::Source;

// Struct raíz — agrupa toda la configuración cargada de múltiples archivos
pub struct AppConfig {
    pub server: ServerConfig,

    // HashMap<String, T> = la clave es el ID (ej: "cred-device42-api")
    // y el valor es el struct sin campo id
    pub credentials: HashMap<String, Credential>,
    pub sources: HashMap<String, Source>,
    pub projects: HashMap<String, GitProject>,
    pub endpoints: HashMap<String, OutputEndpoint>,
}

// Configuración del servidor HTTP — config.yaml
#[derive(Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

// Struct intermedio para parsear config.yaml (solo tiene server por ahora)
#[derive(Deserialize)]
struct ServerFile {
    server: ServerConfig,
}

// Carga toda la configuración desde un directorio.
// Espera encontrar: config.yaml, credentials.yaml, sources.yaml, etc.
// Los archivos opcionales simplemente se ignoran si no existen.
pub fn load_config(config_dir: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let dir = Path::new(config_dir);

    // config.yaml es obligatorio — sin server config no podemos arrancar
    let server_file: ServerFile = load_yaml_file(&dir.join("config.yaml"))?;

    // Los demás son opcionales — si no existen, HashMap vacío
    let credentials = load_optional_yaml(&dir.join("credentials.yaml"))?;
    let sources = load_optional_yaml(&dir.join("sources.yaml"))?;
    let projects = load_optional_yaml(&dir.join("projects.yaml"))?;
    let endpoints = load_optional_yaml(&dir.join("endpoints.yaml"))?;

    Ok(AppConfig {
        server: server_file.server,
        credentials,
        sources,
        projects,
        endpoints,
    })
}

// Lee y parsea un archivo YAML — falla si no existe
fn load_yaml_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let parsed = serde_yaml::from_str(&contents)?;
    Ok(parsed)
}

// Lee y parsea un archivo YAML — devuelve HashMap vacío si no existe
// `T: DeserializeOwned` es un "trait bound": dice que T debe poder deserializarse.
// Es como un type constraint en TypeScript o un Protocol en Python.
fn load_optional_yaml<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<HashMap<String, T>, Box<dyn std::error::Error>> {
    if path.exists() {
        load_yaml_file(path)
    } else {
        Ok(HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Los tests de config necesitan archivos reales en disco.
    // Creamos un directorio temporal con YAML de prueba.

    #[test]
    fn load_config_from_directory() {
        // tempdir: creamos un directorio temporal para el test
        let dir = tempfile::tempdir().unwrap();

        // Escribimos config.yaml mínimo
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        ).unwrap();

        // Escribimos credentials.yaml con formato mapa
        fs::write(
            dir.path().join("credentials.yaml"),
            "cred-test:\n  name: \"Test\"\n  type: \"token\"\n  vault_path: \"secret/test\"\n",
        ).unwrap();

        // dir.path().to_str() convierte el Path a &str
        let cfg = load_config(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 9090);
        assert_eq!(cfg.credentials.len(), 1);
        assert!(cfg.credentials.contains_key("cred-test"));
        // sources.yaml no existe → HashMap vacío, sin error
        assert_eq!(cfg.sources.len(), 0);
    }

    #[test]
    fn load_config_fails_without_server_config() {
        let dir = tempfile::tempdir().unwrap();
        // No creamos config.yaml → debe fallar
        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err()); // is_err() = el Result es un Error
    }
}
