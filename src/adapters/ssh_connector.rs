use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::join_all;
use russh::client;
use russh_keys::{PrivateKey, PublicKey};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::domain::dataset::{Dataset, Group, HostVars};
use crate::ports::connector::{ConnectorError, ConnectorPort, ConnectorResult};

pub struct SshConnector;

impl SshConnector {
    pub fn new() -> Self {
        Self
    }
}

struct SshClientHandler;

#[async_trait]
impl client::Handler for SshClientHandler {
    type Error = russh::Error;

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
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        let script_path = script_path.to_string();
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
                .get("timeout_seconds")
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
                russh_keys::decode_secret_key(&key_data, None).map_err(|e| ConnectorError {
                    message: format!("Failed to decode SSH private key: {}", e),
                    stderr: String::new(),
                    exit_code: None,
                })?;

            let command = build_command(&gather_mode, &fact_path, &script_path);

            info!(
                hosts = hosts.len(),
                concurrency, gather_mode, "SSH connector starting"
            );

            let semaphore = Arc::new(Semaphore::new(concurrency));
            let private_key = Arc::new(private_key);

            let tasks: Vec<_> = hosts
                .into_iter()
                .map(|host| {
                    let sem = Arc::clone(&semaphore);
                    let key = Arc::clone(&private_key);
                    let user = username.clone();
                    let cmd = command.clone();

                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let result = timeout(
                            Duration::from_secs(timeout_secs),
                            execute_on_host(&host, port, &user, &key, &cmd),
                        )
                        .await;

                        match result {
                            Ok(Ok(output)) => {
                                debug!(host = %host, "Gathered successfully");
                                Some((host, output))
                            }
                            Ok(Err(e)) => {
                                warn!(host = %host, error = %e, "SSH execution failed");
                                None
                            }
                            Err(_) => {
                                warn!(host = %host, timeout_secs, "SSH connection timed out");
                                None
                            }
                        }
                    })
                })
                .collect();

            let results = join_all(tasks).await;

            let mut hostvars: HashMap<String, HostVars> = HashMap::new();
            let mut reachable: Vec<String> = Vec::new();

            for result in results {
                match result {
                    Ok(Some((host, output))) => {
                        let vars = parse_host_output(&output, &host);
                        reachable.push(host.clone());
                        hostvars.insert(host, vars);
                    }
                    Ok(None) => {}
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

            info!(
                gathered = reachable.len(),
                "SSH connector finished"
            );

            Ok(Dataset {
                hostvars,
                groups,
                remove_hosts: Vec::new(),
            })
        })
    }
}

fn parse_hosts(config: &HashMap<String, String>) -> Result<Vec<String>, ConnectorError> {
    let hosts_str = config.get("hosts").ok_or_else(|| ConnectorError {
        message: "SSH connector requires 'hosts' in config".into(),
        stderr: String::new(),
        exit_code: None,
    })?;

    let hosts: Vec<String> = hosts_str
        .split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
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

fn build_command(gather_mode: &str, fact_path: &str, script_path: &str) -> String {
    match gather_mode {
        "facts" => {
            format!(
                r#"echo '{{'; first=1; for f in {}/*.fact {}/*.json; do [ -f "$f" ] || continue; name=$(basename "$f" | sed 's/\.[^.]*$//'); content=$(cat "$f"); if [ "$first" = "1" ]; then first=0; else echo ','; fi; printf '"%s": %s' "$name" "$content"; done; echo '}}'"#,
                fact_path, fact_path
            )
        }
        "script" => script_path.to_string(),
        _ => script_path.to_string(),
    }
}

async fn execute_on_host(
    host: &str,
    port: u16,
    username: &str,
    key: &Arc<PrivateKey>,
    command: &str,
) -> Result<String, String> {
    let ssh_config = client::Config {
        ..Default::default()
    };

    let mut session =
        client::connect(Arc::new(ssh_config), (host, port), SshClientHandler)
            .await
            .map_err(|e| format!("Connection to {} failed: {}", host, e))?;

    let auth_ok = session
        .authenticate_publickey(username, Arc::clone(key))
        .await
        .map_err(|e| format!("Auth to {} failed: {}", host, e))?;

    if !auth_ok {
        return Err(format!("Public key authentication rejected by {}", host));
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("Channel open on {} failed: {}", host, e))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| format!("Exec on {} failed: {}", host, e))?;

    let mut output = Vec::new();
    let mut channel = channel;

    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                output.extend_from_slice(&data);
            }
            Some(russh::ChannelMsg::Eof) => break,
            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                if exit_status != 0 {
                    return Err(format!(
                        "Command on {} exited with status {}",
                        host, exit_status
                    ));
                }
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
