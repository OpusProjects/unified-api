use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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
        args: &[String],
        config: &HashMap<String, String>,
        current_dataset: Arc<Dataset>,
    ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let args = args.to_vec();
        let config = config.clone();
        // No clone of the dataset: the Arc is moved into the async block, which
        // shares the cache's copy instead of duplicating it

        Box::pin(async move {
            // Propagate a serialization failure instead of silently sending the
            // script an empty stdin (which would look like an empty dataset).
            let dataset_json =
                serde_json::to_string(&*current_dataset).map_err(|e| EnricherError {
                    message: format!("Failed to serialize dataset: {}", e),
                })?;

            let config_json = serde_json::to_string(&config).map_err(|e| EnricherError {
                message: format!("Failed to serialize config: {}", e),
            })?;

            // The enricher receives:
            // - SOURCE_CONFIG as env var (same as the connector)
            // - The current dataset via stdin (JSON)
            let mut cmd = Command::new(&script_path);
            // Killed if this future is dropped — which is what a timeout
            // does. Without it `timeout_seconds` bounded only how long WE
            // waited: the script kept running, so a wedged one got a fresh
            // copy spawned every interval and none of them ever exited.
            cmd.kill_on_drop(true);
            cmd.args(&args);
            cmd.env("SOURCE_CONFIG", &config_json);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| EnricherError {
                message: format!("Failed to execute enricher '{}': {}", script_path, e),
            })?;

            // Hand the dataset over on a task of its own, so the child's stdout
            // and stderr are drained WHILE we write rather than after.
            //
            // Writing it all first deadlocks as soon as the script writes more
            // than a pipe buffer (64 KiB on Linux) before draining its stdin —
            // an ordinary verbose script logging progress is enough. It blocks
            // on a full stderr, we block on a full stdin, and nothing moves
            // until `timeout_seconds` expires. The dataset is megabytes on a
            // facts source, so the window is not narrow.
            let stdin = child.stdin.take();
            let writer = tokio::spawn(async move {
                if let Some(mut stdin) = stdin {
                    stdin.write_all(dataset_json.as_bytes()).await?;
                }
                // drop(stdin) closes the pipe — the script knows there's no more input
                Ok::<(), std::io::Error>(())
            });

            let output = child.wait_with_output().await.map_err(|e| EnricherError {
                message: format!("Failed to wait for enricher '{}': {}", script_path, e),
            })?;

            let stdin_write = writer.await.map_err(|e| EnricherError {
                message: format!(
                    "Writing to enricher '{}' stdin panicked: {}",
                    script_path, e
                ),
            })?;

            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                // The child's own error wins over any stdin problem: its stderr
                // says why it failed, and a broken pipe is usually just the
                // consequence of it having exited.
                return Err(EnricherError {
                    message: format!(
                        "Enricher '{}' failed with exit code {:?}: {}",
                        script_path,
                        output.status.code(),
                        stderr
                    ),
                });
            }

            // It succeeded. A broken pipe then means the script stopped reading
            // before we finished handing the dataset over — legitimate for one
            // that only needs part of it — so it is not a failure. Anything else
            // is, and would otherwise be swallowed.
            if let Err(e) = stdin_write
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(EnricherError {
                    message: format!("Failed to write to enricher stdin: {}", e),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dataset::HostVars;
    use std::sync::Arc;
    use std::time::Duration;

    // Bigger than a pipe buffer once serialized, so writing it to a child that
    // is not reading actually blocks.
    fn dataset_larger_than_a_pipe_buffer() -> Dataset {
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

        Dataset {
            hostvars,
            groups: HashMap::new(),
            remove_hosts: Vec::new(),
        }
    }

    // A script that writes past a pipe buffer before reading its stdin used to
    // wedge the run: both sides blocked on a full pipe, and nothing moved until
    // the enricher's `timeout_seconds` (300 by default) expired.
    //
    // The timeout here is the test's own, so a regression fails in seconds
    // rather than hanging the suite.
    #[tokio::test]
    async fn a_script_that_talks_before_it_listens_does_not_deadlock() {
        let dataset = Arc::new(dataset_larger_than_a_pipe_buffer());
        assert!(
            serde_json::to_string(&*dataset).unwrap().len() > 64 * 1024,
            "the fixture must be bigger than a pipe buffer or it proves nothing"
        );

        let enricher = ProcessEnricher::new();
        let run = enricher.execute(
            "tests/adapters/out/enrichers/chatty.py",
            &[],
            &HashMap::new(),
            Arc::clone(&dataset),
        );

        let result = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the enricher deadlocked writing stdin while the script wrote stderr");

        let partial = result.expect("the chatty enricher should succeed");
        assert_eq!(partial.hostvars.len(), 2000);
        assert_eq!(partial.hostvars["host00000.example.com"]["chatty"], true);
        // and it carried through what it was given
        assert_eq!(
            partial.hostvars["host00000.example.com"]["datacenter"],
            "aa1"
        );
    }
}
