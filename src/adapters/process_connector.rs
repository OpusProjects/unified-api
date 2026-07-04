use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tokio::process::Command;
use tracing::debug;

use crate::domain::dataset::Dataset;
use crate::ports::connector::{ConnectorError, ConnectorPort, ConnectorResult};

pub struct ProcessConnector;

impl Default for ProcessConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessConnector {
    pub fn new() -> Self {
        Self
    }
}

impl ConnectorPort for ProcessConnector {
    // Box::pin() envuelve el async block en Pin<Box<dyn Future>>
    // para cumplir con la firma del trait.
    // Dentro del Box::pin, el código async es idéntico a antes.
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        // Clonamos los datos para que el async block sea dueño de ellos
        // (el async block puede vivir más que las referencias de entrada)
        let script_path = script_path.to_string();
        let config = config.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            // Serializamos la config a JSON para pasarla como env var
            let config_json = serde_json::to_string(&config).map_err(|e| ConnectorError {
                message: format!("Failed to serialize config: {}", e),
                stderr: String::new(),
                exit_code: None,
            })?;

            // Construimos el comando:
            // - El script como ejecutable
            // - SOURCE_CONFIG con la configuración en JSON
            // - CREDENTIAL_* con cada campo de credencial
            let mut cmd = Command::new(&script_path);
            cmd.env("SOURCE_CONFIG", &config_json);

            // Inyectamos cada credencial como CREDENTIAL_<KEY>=<VALUE>
            // ej: CREDENTIAL_USERNAME=admin, CREDENTIAL_PASSWORD=secret
            let cred_keys: Vec<String> = credentials.keys().cloned().collect();
            if !cred_keys.is_empty() {
                debug!(keys = ?cred_keys, "Injecting credentials");
            }
            for (key, value) in credentials {
                cmd.env(format!("CREDENTIAL_{}", key.to_uppercase()), value);
            }

            // Ejecutamos y capturamos stdout + stderr
            // .output() espera a que el proceso termine y captura todo
            let output = cmd.output().await.map_err(|e| ConnectorError {
                message: format!("Failed to execute script '{}': {}", script_path, e),
                stderr: String::new(),
                exit_code: None,
            })?;

            // Capturamos stderr (para logging/errores)
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            // Verificamos el exit code
            if !output.status.success() {
                return Err(ConnectorError {
                    message: format!(
                        "Script '{}' failed with exit code {:?}",
                        script_path,
                        output.status.code()
                    ),
                    stderr,
                    exit_code: output.status.code(),
                });
            }

            // Parseamos stdout como JSON → Dataset
            let stdout = String::from_utf8_lossy(&output.stdout);
            let dataset: Dataset = serde_json::from_str(&stdout).map_err(|e| ConnectorError {
                message: format!("Failed to parse script output as inventory JSON: {}", e),
                stderr,
                exit_code: Some(0),
            })?;

            Ok(dataset)
        }) // cierra async move
    } // cierra fn execute
} // cierra impl ConnectorPort
