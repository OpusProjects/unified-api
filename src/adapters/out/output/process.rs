use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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
        datasets: &HashMap<String, Arc<Dataset>>,
    ) -> Pin<Box<dyn Future<Output = OutputResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let args = args.to_vec();
        let config = config.clone();
        let params = params.clone();
        // Cloning a map of Arcs only bumps reference counts — the datasets
        // themselves are shared with the cache, not copied
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
            // Killed if this future is dropped — which is what a timeout
            // does. Without it `timeout_seconds` bounded only how long WE
            // waited: the script kept running, so a wedged one got a fresh
            // copy spawned every interval and none of them ever exited.
            cmd.kill_on_drop(true);
            cmd.args(&args);
            cmd.env("ENDPOINT_CONFIG", &config_json);
            cmd.env("ENDPOINT_PARAMS", &params_json);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| OutputError {
                message: format!("Failed to execute output script '{}': {}", script_path, e),
            })?;

            // Written on a task of its own so the child's stdout and stderr are
            // drained WHILE we write. Writing it all first deadlocks as soon as
            // the script writes more than a pipe buffer (64 KiB on Linux) before
            // draining its stdin — it blocks on a full stdout, we block on a
            // full stdin, and nothing moves until `timeout_seconds`. An endpoint
            // is fed every configured source, so its stdin is the largest in the
            // process and its stdout is a whole rendered inventory.
            let stdin = child.stdin.take();
            let writer = tokio::spawn(async move {
                if let Some(mut stdin) = stdin {
                    stdin.write_all(datasets_json.as_bytes()).await?;
                }
                // drop(stdin) closes the pipe — the script knows there's no more input
                Ok::<(), std::io::Error>(())
            });

            let output = child.wait_with_output().await.map_err(|e| OutputError {
                message: format!("Failed to wait for output script '{}': {}", script_path, e),
            })?;

            let stdin_write = writer.await.map_err(|e| OutputError {
                message: format!(
                    "Writing to output script '{}' stdin panicked: {}",
                    script_path, e
                ),
            })?;

            if !output.status.success() {
                // The child's own error wins over any stdin problem: its stderr
                // says why it failed, and a broken pipe is usually just the
                // consequence of it having exited.
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

            // It succeeded. A broken pipe then means the script stopped reading
            // before we finished handing the datasets over — legitimate for one
            // that only needs part of them — so it is not a failure.
            if let Err(e) = stdin_write
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(OutputError {
                    message: format!("Failed to write to output script stdin: {}", e),
                });
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(stdout)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dataset::HostVars;
    use std::time::Duration;

    // Bigger than a pipe buffer once serialized, so handing it to a child that
    // is not reading actually blocks.
    fn datasets_larger_than_a_pipe_buffer() -> HashMap<String, Arc<Dataset>> {
        let hostvars: HashMap<String, HostVars> = (0..2000)
            .map(|i| {
                let vars: HostVars = [
                    ("role".to_string(), serde_json::json!("some-service-role")),
                    ("datacenter".to_string(), serde_json::json!("aa1")),
                ]
                .into_iter()
                .collect();
                (format!("host{:05}.example.com", i), vars)
            })
            .collect();

        [(
            "src-a".to_string(),
            Arc::new(Dataset {
                hostvars,
                groups: HashMap::new(),
                remove_hosts: Vec::new(),
            }),
        )]
        .into_iter()
        .collect()
    }

    // The endpoint counterpart of the enricher's deadlock: a transformer that
    // writes past a pipe buffer before reading its stdin used to wedge the run
    // until `timeout_seconds`, which answers the HTTP caller with a 504 for a
    // script that was working perfectly.
    #[tokio::test]
    async fn a_script_that_talks_before_it_listens_does_not_deadlock() {
        let datasets = datasets_larger_than_a_pipe_buffer();
        assert!(
            serde_json::to_string(&datasets).unwrap().len() > 64 * 1024,
            "the fixture must be bigger than a pipe buffer or it proves nothing"
        );

        let output = ProcessOutput::new();
        let run = output.execute(
            "tests/adapters/out/output/chatty.py",
            &[],
            &HashMap::new(),
            &serde_json::json!({}),
            &datasets,
        );

        let result = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the endpoint deadlocked writing stdin while the script wrote stderr");

        let rendered = result.expect("the chatty transformer should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["hosts"].as_array().unwrap().len(), 2000);
    }
}
