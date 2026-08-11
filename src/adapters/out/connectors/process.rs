use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, warn};

use crate::adapters::out::process_env;

use crate::domain::dataset::Dataset;
use crate::domain::source::OutputFormat;
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
        args: &[String],
        output_format: OutputFormat,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        // We clone the data so the async block owns it
        // (the async block can live longer than the input references)
        let script_path = script_path.to_string();
        let args = args.to_vec();
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
            // - The script as executable, with its CLI arguments (e.g. --list)
            // - SOURCE_CONFIG with the configuration as JSON
            // - CREDENTIAL_* with each credential field
            // Command::args never goes through a shell, so the arguments are
            // passed verbatim — no quoting or injection concerns.
            // The environment is scrubbed (see adapters/out/process_env.rs):
            // the script sees the passthrough list plus what we inject below,
            // not the service's API-key secrets or other sources' credentials.
            let mut cmd = process_env::scrubbed_command(&script_path);
            // Killed if this future is dropped — which is what a timeout
            // does. Without it `timeout_seconds` bounded only how long WE
            // waited: the script kept running, so a wedged one got a fresh
            // copy spawned every interval and none of them ever exited.
            cmd.kill_on_drop(true);
            cmd.args(&args);
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

            // We parse stdout as JSON → Dataset, according to the declared format
            let stdout = String::from_utf8_lossy(&output.stdout);
            let dataset = parse_dataset(&stdout, output_format, &script_path, stderr)?;

            Ok(dataset.into())
        }) // closes async move
    } // closes fn execute
} // closes impl ConnectorPort

fn parse_dataset(
    stdout: &str,
    output_format: OutputFormat,
    script_path: &str,
    stderr: String,
) -> Result<Dataset, ConnectorError> {
    match output_format {
        OutputFormat::Native => {
            let dataset: Dataset = serde_json::from_str(stdout).map_err(|e| ConnectorError {
                message: format!("Failed to parse script output as inventory JSON: {}", e),
                stderr: stderr.clone(),
                exit_code: Some(0),
            })?;

            // Safety net for a misconfigured source: Ansible inventory JSON
            // parses "successfully" as an empty native Dataset (both fields
            // default), which used to look like a mysterious 0-host sync.
            if dataset.hostvars.is_empty()
                && dataset.groups.is_empty()
                && serde_json::from_str::<serde_json::Value>(stdout)
                    .map(|v| Dataset::looks_like_ansible_inventory(&v))
                    .unwrap_or(false)
            {
                warn!(
                    script = %script_path,
                    "output parsed as 0 hosts / 0 groups but contains _meta — \
                     this looks like Ansible inventory JSON; set output_format: \"ansible\" on the source"
                );
            }

            Ok(dataset)
        }
        OutputFormat::Ansible => {
            let value: serde_json::Value =
                serde_json::from_str(stdout).map_err(|e| ConnectorError {
                    message: format!("Failed to parse script output as JSON: {}", e),
                    stderr: stderr.clone(),
                    exit_code: Some(0),
                })?;

            let (dataset, warnings) =
                Dataset::from_ansible_inventory(value).map_err(|e| ConnectorError {
                    message: format!("Failed to convert Ansible inventory output: {}", e),
                    stderr,
                    exit_code: Some(0),
                })?;

            // The domain reports what it dropped/assumed; the adapter logs it
            for warning in warnings {
                warn!(script = %script_path, "{}", warning);
            }

            Ok(dataset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixture prints its whole environment as one host's vars, so the
    // test sees exactly what a real connector script would.
    #[tokio::test]
    async fn a_script_sees_its_credentials_but_not_the_parents_secrets() {
        // set_var is `unsafe` in edition 2024 because other threads may read
        // the environment concurrently; a uniquely named test variable keeps
        // this harmless. It stands in for another source's credential or an
        // API-key secret living in the service's environment.
        unsafe { std::env::set_var("UNIFIED_API_TEST_OTHER_SECRET", "leaked") };

        let mut credentials = HashMap::new();
        credentials.insert("username".to_string(), "admin".to_string());

        let output = ProcessConnector::new()
            .execute(
                "tests/adapters/out/connectors/env_probe.py",
                &[],
                OutputFormat::Native,
                &HashMap::new(),
                &credentials,
            )
            .await
            .expect("the probe connector should succeed");

        let vars = &output.dataset.hostvars["probe"];
        assert_eq!(vars["CREDENTIAL_USERNAME"], "admin");
        assert!(vars.contains_key("SOURCE_CONFIG"));
        assert!(
            !vars.contains_key("UNIFIED_API_TEST_OTHER_SECRET"),
            "the script saw an environment variable that is not its own"
        );
    }
}
