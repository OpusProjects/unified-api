use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::domain::dataset::Dataset;
use crate::ports::output::{OutputError, OutputPort, OutputResult};

pub struct ProcessOutput;

impl Default for ProcessOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessOutput {
    pub fn new() -> Self {
        Self
    }
}

impl OutputPort for ProcessOutput {
    fn execute(
        &self,
        script_path: &str,
        args: &[String],
        config: &HashMap<String, String>,
        params: &serde_json::Value,
        datasets: &HashMap<String, Dataset>,
    ) -> Pin<Box<dyn Future<Output = OutputResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let args = args.to_vec();
        let config = config.clone();
        let params = params.clone();
        let datasets = datasets.clone();

        Box::pin(async move {
            // Propagate a serialization failure instead of silently sending the
            // script an empty stdin (which would look like "no data").
            let datasets_json = serde_json::to_string(&datasets).map_err(|e| OutputError {
                message: format!("Failed to serialize datasets: {}", e),
            })?;

            let config_json = serde_json::to_string(&config).map_err(|e| OutputError {
                message: format!("Failed to serialize config: {}", e),
            })?;

            let params_json = serde_json::to_string(&params).map_err(|e| OutputError {
                message: format!("Failed to serialize params: {}", e),
            })?;

            let mut cmd = Command::new(&script_path);
            cmd.args(&args);
            cmd.env("ENDPOINT_CONFIG", &config_json);
            cmd.env("ENDPOINT_PARAMS", &params_json);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| OutputError {
                message: format!("Failed to execute output script '{}': {}", script_path, e),
            })?;

            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(datasets_json.as_bytes())
                    .await
                    .map_err(|e| OutputError {
                        message: format!("Failed to write to output script stdin: {}", e),
                    })?;
            }

            let output = child.wait_with_output().await.map_err(|e| OutputError {
                message: format!("Failed to wait for output script '{}': {}", script_path, e),
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(OutputError {
                    message: format!(
                        "Output script '{}' failed with exit code {:?}: {}",
                        script_path,
                        output.status.code(),
                        stderr
                    ),
                });
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(stdout)
        })
    }
}
