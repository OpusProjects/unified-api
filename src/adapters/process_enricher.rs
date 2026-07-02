use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::domain::dataset::Dataset;
use crate::ports::enricher::{EnricherError, EnricherPort, EnricherResult};

pub struct ProcessEnricher;

impl ProcessEnricher {
    pub fn new() -> Self {
        Self
    }
}

impl EnricherPort for ProcessEnricher {
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        current_dataset: &Dataset,
    ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let config = config.clone();
        let dataset_json = serde_json::to_string(current_dataset).unwrap_or_default();

        Box::pin(async move {
            let config_json = serde_json::to_string(&config).map_err(|e| EnricherError {
                message: format!("Failed to serialize config: {}", e),
            })?;

            // El enricher recibe:
            // - SOURCE_CONFIG como env var (igual que el connector)
            // - El dataset actual por stdin (JSON)
            let mut cmd = Command::new(&script_path);
            cmd.env("SOURCE_CONFIG", &config_json);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| EnricherError {
                message: format!("Failed to execute enricher '{}': {}", script_path, e),
            })?;

            // Escribimos el dataset por stdin
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(dataset_json.as_bytes()).await.map_err(|e| EnricherError {
                    message: format!("Failed to write to enricher stdin: {}", e),
                })?;
                // drop(stdin) cierra el pipe — el script sabe que no hay más input
            }

            let output = child.wait_with_output().await.map_err(|e| EnricherError {
                message: format!("Failed to wait for enricher '{}': {}", script_path, e),
            })?;

            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                return Err(EnricherError {
                    message: format!(
                        "Enricher '{}' failed with exit code {:?}: {}",
                        script_path,
                        output.status.code(),
                        stderr
                    ),
                });
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let result: Dataset = serde_json::from_str(&stdout).map_err(|e| EnricherError {
                message: format!("Failed to parse enricher output: {}", e),
            })?;

            Ok(result)
        })
    }
}
