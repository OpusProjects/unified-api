use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::domain::dataset::Dataset;
use crate::ports::enricher::{EnricherError, EnricherPort, EnricherResult};

pub struct ProcessEnricher;

impl Default for ProcessEnricher {
    fn default() -> Self {
        Self::new()
    }
}

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
        let current_dataset = current_dataset.clone();

        Box::pin(async move {
            // Propagate a serialization failure instead of silently sending the
            // script an empty stdin (which would look like an empty dataset).
            let dataset_json =
                serde_json::to_string(&current_dataset).map_err(|e| EnricherError {
                    message: format!("Failed to serialize dataset: {}", e),
                })?;

            let config_json = serde_json::to_string(&config).map_err(|e| EnricherError {
                message: format!("Failed to serialize config: {}", e),
            })?;

            // The enricher receives:
            // - SOURCE_CONFIG as env var (same as the connector)
            // - The current dataset via stdin (JSON)
            let mut cmd = Command::new(&script_path);
            cmd.env("SOURCE_CONFIG", &config_json);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| EnricherError {
                message: format!("Failed to execute enricher '{}': {}", script_path, e),
            })?;

            // We write the dataset via stdin
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(dataset_json.as_bytes())
                    .await
                    .map_err(|e| EnricherError {
                        message: format!("Failed to write to enricher stdin: {}", e),
                    })?;
                // drop(stdin) closes the pipe — the script knows there's no more input
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
