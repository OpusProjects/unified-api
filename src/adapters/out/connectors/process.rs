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
    // Box::pin() wraps the async block in Pin<Box<dyn Future>>
    // to satisfy the trait signature.
    // Inside the Box::pin, the async code is identical to before.
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        // We clone the data so the async block owns it
        // (the async block can live longer than the input references)
        let script_path = script_path.to_string();
        let config = config.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            // We serialize the config to JSON to pass it as an env var
            let config_json = serde_json::to_string(&config).map_err(|e| ConnectorError {
                message: format!("Failed to serialize config: {}", e),
                stderr: String::new(),
                exit_code: None,
            })?;

            // We build the command:
            // - The script as executable
            // - SOURCE_CONFIG with the configuration as JSON
            // - CREDENTIAL_* with each credential field
            let mut cmd = Command::new(&script_path);
            cmd.env("SOURCE_CONFIG", &config_json);

            // We inject each credential as CREDENTIAL_<KEY>=<VALUE>
            // e.g. CREDENTIAL_USERNAME=admin, CREDENTIAL_PASSWORD=secret
            let cred_keys: Vec<String> = credentials.keys().cloned().collect();
            if !cred_keys.is_empty() {
                debug!(keys = ?cred_keys, "Injecting credentials");
            }
            for (key, value) in credentials {
                cmd.env(format!("CREDENTIAL_{}", key.to_uppercase()), value);
            }

            // We execute and capture stdout + stderr
            // .output() waits for the process to finish and captures everything
            let output = cmd.output().await.map_err(|e| ConnectorError {
                message: format!("Failed to execute script '{}': {}", script_path, e),
                stderr: String::new(),
                exit_code: None,
            })?;

            // We capture stderr (for logging/errors)
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            // We check the exit code
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

            // We parse stdout as JSON → Dataset
            let stdout = String::from_utf8_lossy(&output.stdout);
            let dataset: Dataset = serde_json::from_str(&stdout).map_err(|e| ConnectorError {
                message: format!("Failed to parse script output as inventory JSON: {}", e),
                stderr,
                exit_code: Some(0),
            })?;

            Ok(dataset)
        }) // closes async move
    } // closes fn execute
} // closes impl ConnectorPort
