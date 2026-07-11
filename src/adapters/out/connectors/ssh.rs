use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::domain::dataset::{Dataset, Group, HostVars};
use crate::domain::source::HostSpec;
use crate::ports::connector::{ConnectorError, ConnectorPort, ConnectorResult};

pub struct SshConnector;

impl Default for SshConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl SshConnector {
    pub fn new() -> Self {
        Self
    }
}

struct SshClientHandler;

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    // We accept any server key: hosts come from trusted config, and there is
    // no known_hosts store to check against in this deployment model.
    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

impl ConnectorPort for SshConnector {
    fn execute(
        &self,
        script_path: &str,
        args: &[String],
        // The SSH connector aggregates per-host outputs into the Dataset
        // itself — there is no single stdout to reinterpret, so the format
        // does not apply here.
        _output_format: crate::domain::source::OutputFormat,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let args = args.to_vec();
        let config = config.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            let hosts = parse_hosts(&config)?;
            let port: u16 = config
                .get("port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(22);
            let concurrency: usize = config
                .get("concurrency")
                .and_then(|c| c.parse().ok())
                .unwrap_or(50);
            let timeout_secs: u64 = config
                .get("ssh_connect_timeout_seconds")
                .and_then(|t| t.parse().ok())
                .unwrap_or(30);
            let gather_mode = config
                .get("gather_mode")
                .cloned()
                .unwrap_or_else(|| "facts".to_string());
            let fact_path = config
                .get("fact_path")
                .cloned()
                .unwrap_or_else(|| "/etc/ansible/facts.d".to_string());

            let username = credentials
                .get("USERNAME")
                .or_else(|| credentials.get("username"))
                .cloned()
                .unwrap_or_else(|| "root".to_string());

            let key_path = credentials
                .get("SSH_KEY_PATH")
                .or_else(|| credentials.get("ssh_key_path"))
                .cloned();

            let key_data = if let Some(path) = &key_path {
                match tokio::fs::read_to_string(path).await {
                    Ok(data) => data,
                    Err(e) => {
                        return Err(ConnectorError {
                            message: format!("Cannot read SSH key at '{}': {}", path, e),
                            stderr: String::new(),
                            exit_code: None,
                        });
                    }
                }
            } else {
                return Err(ConnectorError {
                    message: "No SSH key path provided (need SSH_KEY_PATH in credentials)".into(),
                    stderr: String::new(),
                    exit_code: None,
                });
            };

            let private_key =
                russh::keys::decode_secret_key(&key_data, None).map_err(|e| ConnectorError {
                    message: format!("Failed to decode SSH private key: {}", e),
                    stderr: String::new(),
                    exit_code: None,
                })?;

            let command = build_command(&gather_mode, &fact_path, &script_path, &args);

            info!(
                hosts = hosts.len(),
                concurrency, gather_mode, "SSH connector starting"
            );

            let semaphore = Arc::new(Semaphore::new(concurrency));
            let private_key = Arc::new(private_key);

            let tasks: Vec<_> = hosts
                .into_iter()
                .map(|spec| {
                    let sem = Arc::clone(&semaphore);
                    let key = Arc::clone(&private_key);
                    let user = username.clone();
                    let cmd = command.clone();

                    tokio::spawn(async move {
                        // acquire() only errors if the semaphore is closed, which
                        // never happens here (it lives for the whole fan-out); skip
                        // the host rather than panic the task if that ever changes.
                        let Ok(_permit) = sem.acquire().await else {
                            warn!(host = %spec.name, "semaphore closed, skipping host");
                            return (spec.name, None);
                        };

                        // Try each candidate address in order. A CONNECTION
                        // failure (timeout, refused, DNS) moves to the next
                        // candidate; anything after the TCP connect (auth,
                        // exec) does not — it's the same server answering.
                        let started = std::time::Instant::now();
                        for (attempt, address) in spec.addresses.iter().enumerate() {
                            let result = timeout(
                                Duration::from_secs(timeout_secs),
                                execute_on_host(address, port, &user, &key, &cmd),
                            )
                            .await;

                            match result {
                                Ok(Ok(output)) => {
                                    debug!(
                                        host = %spec.name,
                                        address = %address,
                                        duration_ms = started.elapsed().as_millis() as u64,
                                        "Gathered successfully"
                                    );
                                    return (spec.name, Some(output));
                                }
                                Ok(Err(HostError::Connect(e))) => {
                                    warn!(host = %spec.name, address = %address, attempt = attempt + 1, error = %e, "SSH connection failed");
                                }
                                Err(_elapsed) => {
                                    warn!(host = %spec.name, address = %address, attempt = attempt + 1, timeout_secs, "SSH connection timed out");
                                }
                                Ok(Err(HostError::Other(e))) => {
                                    warn!(host = %spec.name, address = %address, error = %e, "SSH execution failed");
                                    return (spec.name, None);
                                }
                            }
                        }
                        (spec.name, None)
                    })
                })
                .collect();

            let results = join_all(tasks).await;

            let mut hostvars: HashMap<String, HostVars> = HashMap::new();
            let mut reachable: Vec<String> = Vec::new();
            let mut failed: Vec<String> = Vec::new();

            for result in results {
                match result {
                    Ok((host, Some(output))) => {
                        let vars = parse_host_output(&output, &host);
                        reachable.push(host.clone());
                        hostvars.insert(host, vars);
                    }
                    Ok((host, None)) => failed.push(host),
                    Err(e) => {
                        error!(error = %e, "Task join error");
                    }
                }
            }

            let mut groups: HashMap<String, Group> = HashMap::new();
            groups.insert(
                "ssh_gathered".to_string(),
                Group {
                    hosts: reachable.clone(),
                    children: Vec::new(),
                    vars: None,
                },
            );

            // The summary that names the troublemakers: one line to find the
            // hosts that dragged (or dropped out of) the collection.
            if failed.is_empty() {
                info!(gathered = reachable.len(), "SSH connector finished");
            } else {
                failed.sort();
                warn!(
                    gathered = reachable.len(),
                    failed = failed.len(),
                    failed_hosts = ?failed,
                    "SSH connector finished with unreachable hosts"
                );
            }

            Ok(Dataset {
                hostvars,
                groups,
                remove_hosts: Vec::new(),
            })
        })
    }
}

fn parse_hosts(config: &HashMap<String, String>) -> Result<Vec<HostSpec>, ConnectorError> {
    // hosts_spec: JSON list of {name, addresses} — injected by the
    // application layer when the source uses hosts_from_source
    if let Some(spec_json) = config.get("hosts_spec") {
        let specs: Vec<HostSpec> = serde_json::from_str(spec_json).map_err(|e| ConnectorError {
            message: format!("invalid hosts_spec JSON: {}", e),
            stderr: String::new(),
            exit_code: None,
        })?;
        if specs.is_empty() {
            return Err(ConnectorError {
                message: "hosts_spec resolved to zero hosts".into(),
                stderr: String::new(),
                exit_code: None,
            });
        }
        return Ok(specs);
    }

    // Static form: comma-separated hostnames; each host's only candidate
    // address is its own name
    let hosts_str = config.get("hosts").ok_or_else(|| ConnectorError {
        message: "SSH connector requires 'hosts' in config".into(),
        stderr: String::new(),
        exit_code: None,
    })?;

    let hosts: Vec<HostSpec> = hosts_str
        .split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .map(|h| HostSpec {
            addresses: vec![h.clone()],
            name: h,
        })
        .collect();

    if hosts.is_empty() {
        return Err(ConnectorError {
            message: "No hosts provided in config".into(),
            stderr: String::new(),
            exit_code: None,
        });
    }

    Ok(hosts)
}

fn build_command(gather_mode: &str, fact_path: &str, script_path: &str, args: &[String]) -> String {
    match gather_mode {
        // In facts mode the command is fixed — script_args don't apply
        "facts" => {
            format!(
                r#"echo '{{'; first=1; for f in {}/*.fact {}/*.json; do [ -f "$f" ] || continue; name=$(basename "$f" | sed 's/\.[^.]*$//'); if [ -x "$f" ]; then content=$("$f" 2>/dev/null); else content=$(cat "$f"); fi; if [ "$first" = "1" ]; then first=0; else echo ','; fi; printf '"%s": %s' "$name" "$content"; done; echo '}}'"#,
                fact_path, fact_path
            )
        }
        // script (and unknown modes): the script_path is a remote command;
        // script_args are appended to it, space-separated
        _ => {
            if args.is_empty() {
                script_path.to_string()
            } else {
                format!("{} {}", script_path, args.join(" "))
            }
        }
    }
}

// Failure classification drives the address fallback: only Connect errors
// (nothing answered) justify trying the next candidate address.
enum HostError {
    Connect(String),
    Other(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Connect(e) | HostError::Other(e) => write!(f, "{}", e),
        }
    }
}

async fn execute_on_host(
    host: &str,
    port: u16,
    username: &str,
    key: &Arc<PrivateKey>,
    command: &str,
) -> Result<String, HostError> {
    let ssh_config = client::Config {
        ..Default::default()
    };

    let mut session = client::connect(Arc::new(ssh_config), (host, port), SshClientHandler)
        .await
        .map_err(|e| HostError::Connect(format!("Connection to {} failed: {}", host, e)))?;

    let auth_ok = session
        .authenticate_publickey(username, PrivateKeyWithHashAlg::new(Arc::clone(key), None))
        .await
        .map_err(|e| HostError::Other(format!("Auth to {} failed: {}", host, e)))?;

    if !auth_ok.success() {
        return Err(HostError::Other(format!(
            "Public key authentication rejected by {}",
            host
        )));
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| HostError::Other(format!("Channel open on {} failed: {}", host, e)))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| HostError::Other(format!("Exec on {} failed: {}", host, e)))?;

    let mut output = Vec::new();
    let mut channel = channel;

    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                output.extend_from_slice(&data);
            }
            Some(russh::ChannelMsg::Eof) => break,
            Some(russh::ChannelMsg::ExitStatus { exit_status }) if exit_status != 0 => {
                return Err(HostError::Other(format!(
                    "Command on {} exited with status {}",
                    host, exit_status
                )));
            }
            None => break,
            _ => {}
        }
    }

    let stdout = String::from_utf8_lossy(&output).to_string();
    Ok(stdout)
}

fn parse_host_output(output: &str, host: &str) -> HostVars {
    match serde_json::from_str::<HostVars>(output) {
        Ok(vars) => vars,
        Err(e) => {
            warn!(host = %host, error = %e, "Failed to parse host output as JSON, storing as raw");
            let mut vars = HashMap::new();
            vars.insert(
                "raw_output".to_string(),
                serde_json::Value::String(output.to_string()),
            );
            vars.insert(
                "parse_error".to_string(),
                serde_json::Value::String(e.to_string()),
            );
            vars
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_hosts_splits_trims_and_drops_empties() {
        let cfg = config(&[("hosts", " a.example , b.example ,, c.example ")]);
        let hosts = parse_hosts(&cfg).unwrap();
        let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["a.example", "b.example", "c.example"]);
        // static form: each host's only candidate is its own name
        assert_eq!(hosts[0].addresses, vec!["a.example".to_string()]);
    }

    #[test]
    fn parse_hosts_spec_json_takes_precedence() {
        let cfg = config(&[
            ("hosts", "ignored.example"),
            (
                "hosts_spec",
                r#"[{"name": "web01.example.com", "addresses": ["10.0.0.1", "web01.example.com"]}]"#,
            ),
        ]);
        let hosts = parse_hosts(&cfg).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "web01.example.com");
        assert_eq!(
            hosts[0].addresses,
            vec!["10.0.0.1".to_string(), "web01.example.com".to_string()]
        );
    }

    #[test]
    fn parse_hosts_spec_invalid_json_is_an_error() {
        let cfg = config(&[("hosts_spec", "not json")]);
        assert!(
            parse_hosts(&cfg)
                .unwrap_err()
                .message
                .contains("hosts_spec")
        );
    }

    #[test]
    fn parse_hosts_errors_when_missing() {
        let err = parse_hosts(&config(&[])).unwrap_err();
        assert!(err.message.contains("requires 'hosts'"));
    }

    #[test]
    fn parse_hosts_errors_when_only_separators() {
        let err = parse_hosts(&config(&[("hosts", " , , ")])).unwrap_err();
        assert!(err.message.contains("No hosts"));
    }

    #[test]
    fn build_command_script_mode_uses_script_path() {
        assert_eq!(
            build_command("script", "/facts", "/opt/gather.sh", &[]),
            "/opt/gather.sh"
        );
    }

    #[test]
    fn build_command_unknown_mode_falls_back_to_script() {
        assert_eq!(
            build_command("bogus", "/facts", "/opt/gather.sh", &[]),
            "/opt/gather.sh"
        );
    }

    #[test]
    fn build_command_script_mode_appends_args() {
        let args = vec!["--list".to_string(), "-v".to_string()];
        assert_eq!(
            build_command("script", "/facts", "/opt/gather.sh", &args),
            "/opt/gather.sh --list -v"
        );
    }

    #[test]
    fn build_command_facts_mode_ignores_args() {
        let args = vec!["--list".to_string()];
        let cmd = build_command("facts", "/etc/ansible/facts.d", "unused", &args);
        assert!(!cmd.contains("--list"));
    }

    #[test]
    fn build_command_facts_mode_references_fact_path() {
        let cmd = build_command("facts", "/etc/ansible/facts.d", "unused", &[]);
        assert!(cmd.contains("/etc/ansible/facts.d"));
        assert!(cmd.contains("basename"));
    }

    #[test]
    fn parse_host_output_parses_valid_json() {
        let vars = parse_host_output(r#"{"os": "linux", "cpus": 4}"#, "h1");
        assert_eq!(vars.get("os").unwrap(), "linux");
        assert_eq!(vars.get("cpus").unwrap(), 4);
    }

    #[test]
    fn parse_host_output_falls_back_to_raw_on_invalid_json() {
        let vars = parse_host_output("not json", "h1");
        assert_eq!(vars.get("raw_output").unwrap(), "not json");
        assert!(vars.contains_key("parse_error"));
    }
}
