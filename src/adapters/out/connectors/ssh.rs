use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use super::known_hosts::{HostKeyCheck, KnownHosts};
use crate::domain::dataset::{Dataset, Group, HostVars};
use crate::domain::source::HostSpec;
use crate::ports::connector::{ConnectorError, ConnectorOutput, ConnectorPort, ConnectorResult};

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

struct SshClientHandler {
    // None preserves the historical accept-any behaviour (warned about once
    // per sync in execute()); Some verifies against the source's
    // ssh_known_hosts file. The address/port ride along because
    // check_server_key does not receive them.
    verification: Option<HostKeyVerification>,
}

struct HostKeyVerification {
    known_hosts: Arc<KnownHosts>,
    address: String,
    port: u16,
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    // Returning Ok(false) makes russh abort the handshake BEFORE
    // authentication — a mismatched server never sees a signature from our
    // key. The refusal reaches execute_on_host as russh::Error::UnknownKey.
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let Some(v) = &self.verification else {
            // No known_hosts configured: accept any key, as before.
            return Ok(true);
        };
        // russh 0.63 hands the handler either a plain host key or an OpenSSH
        // certificate. known_hosts verification is defined over plain keys —
        // trusting a certificate needs a CA model this connector does not
        // have — so with verification ON, a certificate is refused whole
        // rather than half-checked.
        let server_public_key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => {
                warn!(
                    host = %v.address,
                    "server offered an OpenSSH certificate; ssh_known_hosts \
                     verification covers plain host keys only — refusing to connect"
                );
                return Ok(false);
            }
        };
        match v.known_hosts.check(&v.address, v.port, server_public_key) {
            HostKeyCheck::Known => Ok(true),
            HostKeyCheck::Unknown => {
                warn!(
                    host = %v.address,
                    offered = %server_public_key.fingerprint(HashAlg::Sha256),
                    "no known_hosts entry for this host; refusing to connect"
                );
                Ok(false)
            }
            HostKeyCheck::Changed { expected } => {
                warn!(
                    host = %v.address,
                    offered = %server_public_key.fingerprint(HashAlg::Sha256),
                    expected = ?expected,
                    "HOST KEY MISMATCH — refusing to connect (possible MITM, or a reinstalled host whose known_hosts entry is stale)"
                );
                Ok(false)
            }
        }
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
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        let script_path = script_path.to_string();
        let args = args.to_vec();
        let config = config.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            let hosts = narrow_to_scope(parse_hosts(&config)?, &config)?;
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
            let plan = plan(&script_path, &args, &config).await?;
            let legacy_algorithms = config
                .get("ssh_legacy_algorithms")
                .is_some_and(|v| v == "true");

            // Parsed once per sync, shared across the fan-out. Re-read on
            // every sync ON PURPOSE: rotating a host key means editing the
            // file, and that must not require a restart. An unreadable file
            // fails the whole sync — silently gathering nothing (every host
            // refused as unknown) would look like a fleet-wide outage, and
            // silently verifying nothing would betray the setting.
            let known_hosts = match config.get("ssh_known_hosts") {
                Some(path) => match tokio::fs::read_to_string(path).await {
                    Ok(content) => Some(Arc::new(KnownHosts::parse(&content))),
                    Err(e) => {
                        return Err(ConnectorError {
                            message: format!("Cannot read ssh_known_hosts '{}': {}", path, e),
                            stderr: String::new(),
                            exit_code: None,
                        });
                    }
                },
                None => {
                    warn!(
                        "SSH host keys are NOT verified for this sync — any server \
                         answering on the port is trusted; set ssh_known_hosts on the \
                         source to pin them"
                    );
                    None
                }
            };

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

            info!(
                hosts = hosts.len(),
                concurrency,
                what = plan.what,
                "SSH connector starting"
            );
            let facts = plan.facts;
            let stdin = plan.stdin.map(Arc::new);
            let command = plan.command;

            let semaphore = Arc::new(Semaphore::new(concurrency));
            let private_key = Arc::new(private_key);

            // A JoinSet rather than loose `tokio::spawn` handles: dropping it
            // aborts whatever is still running, and dropping it is exactly what
            // the source-level `timeout_seconds` does to this whole future.
            //
            // Detached tasks kept gathering after the sync had given up on them.
            // On a large fleet that is thousands of SSH sessions still being
            // opened against a datacenter for a result nobody will read — the
            // opposite of what a timeout is for.
            let mut tasks = tokio::task::JoinSet::new();
            for spec in hosts {
                {
                    let sem = Arc::clone(&semaphore);
                    let key = Arc::clone(&private_key);
                    let user = username.clone();
                    let cmd = command.clone();
                    let stdin = stdin.clone();
                    let known_hosts = known_hosts.clone();

                    tasks.spawn(async move {
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
                                execute_on_host(
                                    address,
                                    port,
                                    &user,
                                    &key,
                                    &cmd,
                                    stdin.as_deref().map(Vec::as_slice),
                                    legacy_algorithms,
                                    known_hosts.clone(),
                                ),
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
                    });
                }
            }

            let mut hostvars: HashMap<String, HostVars> = HashMap::new();
            let mut reachable: Vec<String> = Vec::new();
            let mut failed: Vec<String> = Vec::new();

            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok((host, Some(output))) => {
                        let vars = if facts {
                            parse_facts_output(&output)
                        } else {
                            parse_host_output(&output, &host)
                        };
                        reachable.push(host.clone());
                        hostvars.insert(host, vars);
                    }
                    Ok((host, None)) => failed.push(host),
                    Err(e) => {
                        error!(error = %e, "Task join error");
                    }
                }
            }

            // Sorted because results now arrive in COMPLETION order rather than
            // spawn order, and a group whose member list reshuffles on every
            // sync would change the dataset's ETag without the data changing.
            reachable.sort();

            let mut groups: HashMap<String, Group> = HashMap::new();
            groups.insert(
                "ssh_gathered".to_string(),
                Group {
                    hosts: reachable.clone(),
                    children: Vec::new(),
                    vars: None,
                },
            );

            // Auto-groups from fact keys (like Ansible keyed_groups):
            // a group per top-level key containing hosts that have it.
            let mut key_groups: HashMap<String, Vec<String>> = HashMap::new();
            for (host, vars) in &hostvars {
                for key in vars.keys() {
                    key_groups
                        .entry(key.clone())
                        .or_default()
                        .push(host.clone());
                }
            }
            for (name, mut hosts) in key_groups {
                hosts.sort();
                groups.insert(
                    name,
                    Group {
                        hosts,
                        children: Vec::new(),
                        vars: None,
                    },
                );
            }

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

            Ok(ConnectorOutput {
                dataset: Dataset {
                    hostvars,
                    groups,
                    remove_hosts: Vec::new(),
                },
                ages: None,
                advertised_scope: None,
                // Named here so a full sync can keep their last-known data
                // rather than dropping them for a failure that is ours, not
                // theirs: one saturated batch of workers took every host in it
                // out of the inventory until the next run.
                unreachable: failed,
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

// Narrow the host list to what a host-scoped sync asked for.
//
// The scope arrives in the config (application::sync puts `scope` and `target`
// there for every connector). This connector used to ignore it and read only
// `hosts`/`hosts_spec`, so a `POST /sync?host=one-box` opened an SSH session to
// every host in the datacenter and then kept a single one — the cost of a full
// gather for one host's worth of data.
//
// Group scope is deliberately not narrowed here: HostSpec carries a name and
// addresses, not group membership, so this connector has no way to tell which
// hosts belong to a group. A group-scoped sync still gathers everything and
// lets the cache apply the group slice.
fn narrow_to_scope(
    hosts: Vec<HostSpec>,
    config: &HashMap<String, String>,
) -> Result<Vec<HostSpec>, ConnectorError> {
    if config.get("scope").map(String::as_str) != Some("host") {
        return Ok(hosts);
    }

    let target = config.get("target").map(String::as_str).unwrap_or_default();
    let wanted: Vec<&str> = target
        .split(',')
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .collect();

    let narrowed: Vec<HostSpec> = hosts
        .into_iter()
        .filter(|h| wanted.contains(&h.name.as_str()))
        .collect();

    // Naming a host this source does not know about is a configuration or
    // consumer mistake, and gathering nothing would report a bland success.
    // The message names what was asked for, because the usual cause is a
    // hostname that differs between the inventory and the caller.
    if narrowed.is_empty() {
        return Err(ConnectorError {
            message: format!(
                "host scope '{}' matched none of this source's hosts",
                target
            ),
            stderr: String::new(),
            exit_code: None,
        });
    }

    Ok(narrowed)
}

// What one sync runs on every host, decided once from the connector's
// location (ports::connector::SSH_LOCATION_CONFIG_KEY):
// - host: every file the path matches there (one path matches itself) is
//   run if executable, read if not, and becomes a fact named after it
// - instance: the script file here, streamed to the host's interpreter
// - absent: the connector's own script, streamed the same way
struct Plan {
    command: String,
    // Written to the remote command's standard input, then closed.
    stdin: Option<Vec<u8>>,
    // Output is a stream of facts files (parse_facts_output), not one JSON.
    facts: bool,
    // For the log line.
    what: &'static str,
}

async fn plan(
    script_path: &str,
    args: &[String],
    config: &HashMap<String, String>,
) -> Result<Plan, ConnectorError> {
    let fail = |message: String| ConnectorError {
        message,
        stderr: String::new(),
        exit_code: None,
    };
    match config
        .get(crate::ports::connector::SSH_LOCATION_CONFIG_KEY)
        .map(String::as_str)
    {
        Some("host") => Ok(Plan {
            command: facts_command(script_path, args),
            stdin: None,
            facts: true,
            what: "files on each host",
        }),
        Some("instance") => {
            // Read at every sync, like a script connector's file: a push to
            // the project is live at the next run, no restart.
            let script = tokio::fs::read(script_path)
                .await
                .map_err(|e| fail(format!("Cannot read ssh script '{script_path}': {e}")))?;
            Ok(Plan {
                command: stream_command(&script, args)
                    .map_err(|e| fail(format!("ssh script '{script_path}': {e}")))?,
                stdin: Some(script),
                facts: false,
                what: "a script streamed to each host",
            })
        }
        _ => {
            // Written in the item: the same as a file from here, without
            // the file.
            let script = script_path.as_bytes().to_vec();
            Ok(Plan {
                command: stream_command(&script, args)
                    .map_err(|e| fail(format!("the connector's script: {e}")))?,
                stdin: Some(script),
                facts: false,
                what: "the connector's script, streamed to each host",
            })
        }
    }
}

// Every file matching `pattern` on the host (a plain path matches itself),
// the way Ansible reads local facts: an executable is run and its output
// used (with the connector's script_args, quoted), anything else is read. Each file is announced by a record separator (0x1e, which neither
// JSON nor INI carries) and its name without the extension, so a file that
// is not valid JSON spoils only its own fact. The pattern goes in unquoted,
// so the remote shell expands it; Connector::errors allowed only characters
// that shell reads as a path or a pattern.
fn facts_command(pattern: &str, args: &[String]) -> String {
    under_sh(
        &format!(
            r#"for f in {pattern}; do [ -f "$f" ] || continue; n=${{f##*/}}; m=${{n%.*}}; [ -n "$m" ] && n=$m; printf "\036%s\n" "$n"; if [ -x "$f" ]; then "$f" "$@" </dev/null 2>/dev/null; else cat "$f"; fi; echo; done"#
        ),
        args,
    )
}

// A remote command that /bin/sh runs whatever the account's login shell is
// (csh and fish cannot read sh syntax): `inner` in single quotes, with `args`
// quoted after it as $1, $2… The words written into `inner` (a host path, an
// interpreter from a #! line) are checked to be plain, so it never holds a
// single quote.
fn under_sh(inner: &str, args: &[String]) -> String {
    debug_assert!(!inner.contains('\''), "{inner}");
    let mut line = format!("exec /bin/sh -c '{inner}' sh");
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    line
}

use crate::domain::connector::shell_quote;

// The remote command that runs a script sent on standard input as a FILE,
// as it would run from a checkout: the host's /bin/sh writes what arrives to
// a private temporary file, runs it with the interpreter its #! line names
// (with that line's flags; env and its own options skipped) or, without one,
// the account's login shell, as an ssh command line is run, and removes it
// when the run ends — also when the session is cut by the timeout (HUP).
// A file, not `sh -c "$script"`: $0, $BASH_SOURCE, __file__ and node's
// arguments are what the script expects, an argument starting with "-" is
// never taken for the interpreter's own option, and a `#!/bin/sh -` line
// works. The script runs with its standard input empty, so nothing in it
// waits on the connection; and nothing is printed before the whole script
// has arrived, so the output cannot fill the channel while it is sent.
fn stream_command(script: &[u8], args: &[String]) -> Result<String, String> {
    let run = match interpreter(script)? {
        Some((program, flags)) => {
            let mut run = program;
            for flag in flags {
                run.push(' ');
                run.push_str(&flag);
            }
            run
        }
        None => r#""${SHELL:-/bin/sh}""#.to_string(),
    };
    // Step by step, on the host:
    // - files an earlier run left (the timeout cut its session while the
    //   script hung: sshd sends no signal then, so its trap never ran) are
    //   removed once a day old; POSIX find only, no -maxdepth, for AIX;
    // - a private file: mktemp, or where there is none (base AIX) one named
    //   after this shell, created only if absent (noclobber), mode 600;
    // - the script, then its size checked against what was sent: a session
    //   cut mid-send ends `cat` as cleanly as the end of the script does,
    //   and a part of a script is never run;
    // - run, standard input empty; the file removed on exit.
    let size = script.len();
    Ok(under_sh(
        &format!(
            r#"d="${{TMPDIR:-/tmp}}"; find "$d/." ! -name . -prune -name "unified-api.*" -user "$(id -u)" -mtime +0 -exec rm -f {{}} + 2>/dev/null; t=$(mktemp "$d/unified-api.XXXXXX" 2>/dev/null) || {{ t="$d/unified-api.$$"; (set -C; umask 077; : >"$t") || exit 1; }}; trap "rm -f -- \"$t\"" EXIT; trap "exit 129" HUP; trap "exit 130" INT; trap "exit 143" TERM; cat >"$t" || exit 1; [ "$(wc -c <"$t" | tr -d " ")" = {size} ] || {{ echo "unified-api: the script arrived incomplete" >&2; exit 75; }}; {run} "$t" "$@" </dev/null"#
        ),
        args,
    ))
}

// The interpreter a script's #! line names, and the flags it gives it:
// `#!/bin/bash -e` is ("/bin/bash", ["-e"]); `#!/usr/bin/env -S python3 -u`
// is ("python3", ["-u"]). None without a #! line. Every word must be a plain
// name or flag: it is written into the remote command line.
fn interpreter(script: &[u8]) -> Result<Option<(String, Vec<String>)>, String> {
    let first = script.split(|b| *b == b'\n').next().unwrap_or_default();
    let first = String::from_utf8_lossy(first);
    let Some(rest) = first.strip_prefix("#!") else {
        return Ok(None);
    };
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    if words.first().is_some_and(|w| w.ends_with("/env")) {
        words.remove(0);
        while words.first().is_some_and(|w| w.starts_with('-')) {
            words.remove(0);
        }
    }
    let plain = |w: &str| {
        w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '='))
    };
    match words.split_first() {
        Some((program, flags)) if plain(program) && flags.iter().all(|f| plain(f)) => Ok(Some((
            program.to_string(),
            flags.iter().map(|f| f.to_string()).collect(),
        ))),
        _ => Err(format!(
            "its #! line '{}' names no plain program",
            first.trim()
        )),
    }
}

// Failure classification drives the address fallback: only Connect errors
// (nothing answered) justify trying the next candidate address.
#[derive(Debug)]
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

// Build the client algorithm preferences. The default excludes every SHA-1
// construct; `ssh_legacy_algorithms: "true"` APPENDS the legacy KEX and MAC
// algorithms (always last, so a modern server still negotiates the modern
// ones) for hosts of the OpenSSH 5.x era (EL6) that have no hmac-sha2.
fn client_config(legacy_algorithms: bool) -> client::Config {
    let mut config = client::Config::default();
    if legacy_algorithms {
        let mut kex = config.preferred.kex.to_vec();
        kex.extend_from_slice(&[
            russh::kex::DH_GEX_SHA1,
            russh::kex::DH_G14_SHA1,
            russh::kex::DH_G1_SHA1,
        ]);
        config.preferred.kex = kex.into();

        let mut mac = config.preferred.mac.to_vec();
        mac.extend_from_slice(&[russh::mac::HMAC_SHA1_ETM, russh::mac::HMAC_SHA1]);
        config.preferred.mac = mac.into();
    }
    config
}

// Which RSA signature hashes to attempt, in order. `negotiated` is what
// best_supported_rsa_hash() returned:
// - Some(alg): the server advertised server-sig-algs — trust it (covers
//   both modern servers wanting rsa-sha2 and old ones advertising ssh-rsa)
// - None: no extension (pre-OpenSSH-7.2, e.g. EL6). Try SHA-256 first —
//   most servers without the extension still accept it — and fall back to
//   the legacy SHA-1 signature if the server rejects the auth.
// Hardcoding None (SHA-1) here is what silently locked us out of every
// RHEL9-era host while the same key worked with the OpenSSH client.
fn rsa_hash_candidates(
    negotiated: Option<Option<russh::keys::HashAlg>>,
) -> Vec<Option<russh::keys::HashAlg>> {
    match negotiated {
        Some(alg) => vec![alg],
        None => vec![Some(russh::keys::HashAlg::Sha256), None],
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_on_host(
    host: &str,
    port: u16,
    username: &str,
    key: &Arc<PrivateKey>,
    command: &str,
    stdin: Option<&[u8]>,
    legacy_algorithms: bool,
    known_hosts: Option<Arc<KnownHosts>>,
) -> Result<String, HostError> {
    let ssh_config = client_config(legacy_algorithms);

    let handler = SshClientHandler {
        verification: known_hosts.map(|kh| HostKeyVerification {
            known_hosts: kh,
            address: host.to_string(),
            port,
        }),
    };

    let mut session = client::connect(Arc::new(ssh_config), (host, port), handler)
        .await
        .map_err(|e| match e {
            // Our own handler said no. Classified as Other, not Connect:
            // the address-fallback loop retries Connect failures on the
            // next candidate, and a refused key is a decision, not a
            // network hiccup. The handler already logged the fingerprints.
            russh::Error::UnknownKey => HostError::Other(format!(
                "Host key verification failed for {} (see the preceding warning for fingerprints)",
                host
            )),
            e => HostError::Connect(format!("Connection to {} failed: {}", host, e)),
        })?;

    // Non-RSA keys (ed25519, ecdsa) don't have a signature hash to negotiate;
    // skipping the lookup also skips its up-to-1s wait for the server's
    // EXT_INFO message.
    let hash_candidates = if key.algorithm().is_rsa() {
        let negotiated = session.best_supported_rsa_hash().await.map_err(|e| {
            HostError::Other(format!(
                "Negotiating RSA signature hash with {} failed: {}",
                host, e
            ))
        })?;
        rsa_hash_candidates(negotiated)
    } else {
        vec![None]
    };

    let mut authenticated = false;
    for hash_alg in hash_candidates {
        let auth_ok = session
            .authenticate_publickey(
                username,
                PrivateKeyWithHashAlg::new(Arc::clone(key), hash_alg),
            )
            .await
            .map_err(|e| HostError::Other(format!("Auth to {} failed: {}", host, e)))?;
        if auth_ok.success() {
            authenticated = true;
            break;
        }
        debug!(host = %host, hash_alg = ?hash_alg, "publickey auth rejected with this signature hash");
    }

    if !authenticated {
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
    // A script streamed from here: written to the command's standard input,
    // which is then closed so the interpreter knows the script ended.
    if let Some(script) = stdin {
        channel.data(script).await.map_err(|e| {
            HostError::Other(format!("Sending the script to {} failed: {}", host, e))
        })?;
        channel.eof().await.map_err(|e| {
            HostError::Other(format!("Sending the script to {} failed: {}", host, e))
        })?;
    }

    let mut output = Vec::new();
    let mut exit_status: Option<u32> = None;
    let mut channel = channel;

    // Read until the channel closes rather than stopping at EOF.
    //
    // The exit status arrives as a message of its own, and the protocol fixes no
    // order between it and EOF — EOF means "no more output", the exit status
    // means "the process finished", and a command that closes stdout before
    // exiting produces them the other way round. Breaking on EOF could therefore
    // return before the status was ever seen, and a command that FAILED was then
    // reported as a successful gather whose output happens to be an error
    // message: `parse_host_output` stores that as the host's variables, so the
    // inventory gains a host described by a shell error rather than a host
    // marked unreachable (which would have kept its last known good data).
    //
    // Waiting for the close costs nothing on a well-behaved server, which sends
    // it immediately, and a server that never closes is bounded by the per-host
    // timeout this function is already called under.
    while let Some(message) = channel.wait().await {
        match message {
            russh::ChannelMsg::Data { data } => output.extend_from_slice(&data),
            russh::ChannelMsg::ExitStatus {
                exit_status: status,
            } => exit_status = Some(status),
            _ => {}
        }
    }

    finish_host_output(output, exit_status, host)
}

// What a finished channel amounts to. Separated from the read loop so the rule
// can be tested without an SSH server.
//
// No status reported at all is treated as success: some servers never send one,
// and that was the behaviour before this was checked properly — newly failing
// every host on such a server would be a worse bug than the one being fixed.
fn finish_host_output(
    output: Vec<u8>,
    exit_status: Option<u32>,
    host: &str,
) -> Result<String, HostError> {
    match exit_status {
        Some(status) if status != 0 => Err(HostError::Other(format!(
            "Command on {} exited with status {}",
            host, status
        ))),
        _ => Ok(String::from_utf8_lossy(&output).to_string()),
    }
}

// The facts files one host printed (facts_command): each one becomes a
// fact named after its file. JSON first, then INI (a section is an object of
// strings, keys lowercased, as Ansible's configparser reads them); a file
// that is neither becomes the same error string Ansible would put there,
// rather than dropping the host.
fn parse_facts_output(output: &str) -> HostVars {
    let mut vars = HashMap::new();
    for record in output.split('\u{1e}').skip(1) {
        let (name, content) = record.split_once('\n').unwrap_or((record, ""));
        if name.is_empty() {
            continue;
        }
        vars.insert(name.to_string(), parse_fact(content.trim()));
    }
    vars
}

fn parse_fact(content: &str) -> serde_json::Value {
    if let Ok(value) = serde_json::from_str(content) {
        return value;
    }
    parse_ini(content).unwrap_or_else(|| {
        serde_json::Value::String("error loading fact - please check content".to_string())
    })
}

fn parse_ini(content: &str) -> Option<serde_json::Value> {
    let mut sections = serde_json::Map::new();
    let mut current: Option<String> = None;
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(section.trim().to_string());
            sections
                .entry(section.trim().to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            continue;
        }
        // A key before any section, or a line that is not `key = value`:
        // not INI.
        let section = current.as_ref()?;
        let (key, value) = line.split_once(['=', ':'])?;
        if let Some(serde_json::Value::Object(map)) = sections.get_mut(section) {
            map.insert(
                key.trim().to_lowercase(),
                serde_json::Value::String(value.trim().to_string()),
            );
        }
    }
    (!sections.is_empty()).then_some(serde_json::Value::Object(sections))
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
    fn rsa_candidates_trust_the_server_advertisement() {
        use russh::keys::HashAlg;
        // modern server advertising rsa-sha2-512
        assert_eq!(
            rsa_hash_candidates(Some(Some(HashAlg::Sha512))),
            vec![Some(HashAlg::Sha512)]
        );
        // old server advertising only ssh-rsa: SHA-1, and ONLY SHA-1
        assert_eq!(rsa_hash_candidates(Some(None)), vec![None]);
    }

    #[test]
    fn rsa_candidates_without_extension_try_sha256_then_sha1() {
        use russh::keys::HashAlg;
        assert_eq!(rsa_hash_candidates(None), vec![Some(HashAlg::Sha256), None]);
    }

    #[test]
    fn legacy_algorithms_flag_appends_sha1_kex_and_macs() {
        let modern = client_config(false);
        assert!(!modern.preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(!modern.preferred.mac.contains(&russh::mac::HMAC_SHA1));

        let legacy = client_config(true);
        assert!(legacy.preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(legacy.preferred.mac.contains(&russh::mac::HMAC_SHA1));
        // appended at the END: a modern server still picks the modern ones
        assert_eq!(legacy.preferred.kex.first(), modern.preferred.kex.first());
        assert_eq!(legacy.preferred.mac.first(), modern.preferred.mac.first());
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

    fn spec(name: &str) -> HostSpec {
        HostSpec {
            name: name.to_string(),
            addresses: vec![name.to_string()],
        }
    }

    #[test]
    fn narrow_to_scope_is_a_passthrough_without_host_scope() {
        let hosts = vec![spec("a.example"), spec("b.example")];
        // no scope at all
        assert_eq!(
            narrow_to_scope(hosts.clone(), &config(&[])).unwrap().len(),
            2
        );
        // a group scope is NOT narrowed here — HostSpec has no group membership
        let group = config(&[("scope", "group"), ("target", "web")]);
        assert_eq!(narrow_to_scope(hosts, &group).unwrap().len(), 2);
    }

    #[test]
    fn narrow_to_scope_keeps_only_the_targeted_host() {
        let hosts = vec![spec("a.example"), spec("b.example"), spec("c.example")];
        let cfg = config(&[("scope", "host"), ("target", "b.example")]);
        let narrowed = narrow_to_scope(hosts, &cfg).unwrap();
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].name, "b.example");
    }

    #[test]
    fn narrow_to_scope_accepts_a_comma_separated_target() {
        let hosts = vec![spec("a.example"), spec("b.example"), spec("c.example")];
        let cfg = config(&[("scope", "host"), ("target", " c.example , a.example ")]);
        let names: Vec<String> = narrow_to_scope(hosts, &cfg)
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        // the source's own order is preserved, not the target's
        assert_eq!(names, vec!["a.example", "c.example"]);
    }

    #[test]
    fn narrow_to_scope_preserves_the_candidate_addresses() {
        let hosts = vec![HostSpec {
            name: "web01.example.com".to_string(),
            addresses: vec!["10.0.0.1".to_string(), "web01.example.com".to_string()],
        }];
        let cfg = config(&[("scope", "host"), ("target", "web01.example.com")]);
        let narrowed = narrow_to_scope(hosts, &cfg).unwrap();
        assert_eq!(
            narrowed[0].addresses,
            vec!["10.0.0.1".to_string(), "web01.example.com".to_string()]
        );
    }

    #[test]
    fn narrow_to_scope_errors_when_the_target_is_unknown() {
        let hosts = vec![spec("a.example")];
        let cfg = config(&[("scope", "host"), ("target", "ghost.example")]);
        let err = narrow_to_scope(hosts, &cfg).unwrap_err();
        assert!(err.message.contains("ghost.example"), "{}", err.message);
        assert!(err.message.contains("matched none"), "{}", err.message);
    }

    #[test]
    fn a_zero_exit_status_yields_the_output() {
        let result = finish_host_output(b"{\"a\": 1}".to_vec(), Some(0), "web01.example");
        assert_eq!(result.unwrap(), "{\"a\": 1}");
    }

    // The bug this rule exists for: the command failed, but it had written
    // something first. That output used to be returned and stored as the host's
    // facts, so a broken command became a host described by its own error text.
    #[test]
    fn a_failed_command_is_an_error_even_when_it_produced_output() {
        let result = finish_host_output(
            b"bash: /etc/ansible/facts.d: No such file".to_vec(),
            Some(127),
            "web01.example",
        );
        let error = result.expect_err("a non-zero exit status must fail the host");
        assert!(error.to_string().contains("127"), "error was: {}", error);
        assert!(
            error.to_string().contains("web01.example"),
            "error was: {}",
            error
        );
    }

    // Some servers never send an exit status. Failing every host on one of those
    // would be a worse bug than the one this check fixes.
    #[test]
    fn no_reported_exit_status_is_not_a_failure() {
        let result = finish_host_output(b"{\"a\": 1}".to_vec(), None, "web01.example");
        assert_eq!(result.unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn output_that_is_not_utf8_is_kept_lossily_rather_than_failing() {
        let result = finish_host_output(vec![0x7b, 0xff, 0x7d], Some(0), "web01.example");
        assert!(result.is_ok());
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

    #[test]
    fn facts_files_become_facts_named_after_their_file() {
        let output = "\u{1e}osinfo\n{\"id\": \"rhel\"}\n\n\u{1e}rack\n[location]\nRow = 4\n# comment\nslot: 12\n\n\u{1e}broken\nnot json or ini\n";
        let vars = parse_facts_output(output);
        assert_eq!(vars["osinfo"], serde_json::json!({"id": "rhel"}));
        // INI as Ansible reads it: sections of strings, keys lowercased.
        assert_eq!(
            vars["rack"],
            serde_json::json!({"location": {"row": "4", "slot": "12"}})
        );
        // One bad file spoils only its own fact.
        assert_eq!(vars["broken"], "error loading fact - please check content");
        assert_eq!(vars.len(), 3);
    }

    // Runs a remote command as an ssh server would, here: through /bin/sh
    // (standing in for the login shell), with `stdin` on its standard input
    // and temporary files under `tmp`.
    fn run_remote(command: &str, stdin: &[u8], tmp: &std::path::Path) -> String {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("TMPDIR", tmp)
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let output = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn a_streamed_script_runs_as_a_file_and_leaves_none() {
        let tmp = tempfile::tempdir().unwrap();
        let args = ["--x".to_string(), "it's".to_string()];
        let run = |script: &str| {
            run_remote(
                &stream_command(script.as_bytes(), &args).unwrap(),
                script.as_bytes(),
                tmp.path(),
            )
        };
        // A command reading standard input cannot swallow what follows it;
        // arguments arrive whole, starting with "-" or holding a quote.
        assert_eq!(
            run("echo start\ncat\nread x\necho \"after $1 $2\"\n"),
            "start\nafter --x it's\n"
        );
        // As a file: $0 names it, and a `#!/bin/sh -` line works.
        assert!(
            run("#!/bin/sh -\ncase $0 in */unified-api.*) echo file;; esac\n").contains("file")
        );
        // perl takes its arguments as arguments, never as its own switches.
        assert_eq!(run("#!/usr/bin/perl\nprint \"@ARGV\\n\";\n"), "--x it's\n");
        // Nothing is left behind.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    fn run_remote_with(
        command: &str,
        stdin: &[u8],
        tmp: &std::path::Path,
        path: Option<&std::path::Path>,
    ) -> (i32, String) {
        use std::io::Write;
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(command)
            .env("TMPDIR", tmp)
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(path) = path {
            cmd.env("PATH", path);
        }
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    #[test]
    fn a_script_cut_short_on_the_way_is_never_run() {
        let tmp = tempfile::tempdir().unwrap();
        let script = b"echo first\necho second\n";
        let command = stream_command(script, &[]).unwrap();
        // The session ends after half of it: nothing of it runs.
        let (code, out) = run_remote_with(&command, &script[..10], tmp.path(), None);
        assert_eq!(code, 75);
        assert_eq!(out, "");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_host_without_mktemp_still_runs_the_script() {
        let tmp = tempfile::tempdir().unwrap();
        // A PATH with what the command needs, but no mktemp.
        let bin = tempfile::tempdir().unwrap();
        for tool in ["cat", "wc", "tr", "find", "id", "rm", "sh"] {
            let found = ["/usr/bin", "/bin"]
                .iter()
                .map(|d| std::path::Path::new(d).join(tool))
                .find(|p| p.exists())
                .unwrap();
            std::os::unix::fs::symlink(found, bin.path().join(tool)).unwrap();
        }
        let script = b"echo ran\n";
        let command = stream_command(script, &[]).unwrap();
        let (code, out) = run_remote_with(&command, script, tmp.path(), Some(bin.path()));
        assert_eq!((code, out.as_str()), (0, "ran\n"));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_day_old_leftover_is_removed_and_a_fresh_one_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("unified-api.stale1");
        let fresh = tmp.path().join("unified-api.running");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&fresh, "x").unwrap();
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(two_days)
            .unwrap();
        let script = b"echo ok\n";
        let (code, _) = run_remote_with(
            &stream_command(script, &[]).unwrap(),
            script,
            tmp.path(),
            None,
        );
        assert_eq!(code, 0);
        assert!(!old.exists(), "a day-old leftover goes");
        assert!(fresh.exists(), "a run still going keeps its file");
    }

    #[test]
    fn the_interpreter_comes_from_the_hash_bang_line() {
        let cmd = |script: &[u8]| stream_command(script, &[]).unwrap();
        // No #! line: the account's login shell, as an ssh command line.
        assert!(cmd(b"echo hi\n").contains(r#""${SHELL:-/bin/sh}" "$t""#));
        // Flags kept; env and its own options skipped.
        assert!(cmd(b"#!/bin/bash -e\n").contains(r#"/bin/bash -e "$t""#));
        assert!(cmd(b"#!/usr/bin/env -S python3 -u\n").contains(r#"python3 -u "$t""#));
        // Whatever the login shell, /bin/sh runs it.
        assert!(cmd(b"echo hi\n").starts_with("exec /bin/sh -c '"));
        // Not a plain program name: refused.
        assert!(
            stream_command(b"#!/bin/sh; rm -rf /\n", &[])
                .unwrap_err()
                .contains("no plain program")
        );
    }

    #[test]
    fn the_facts_command_reads_every_matching_file() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("owner.fact"), "{\"team\": \"a\"}\n").unwrap();
        std::fs::write(dir.path().join(".local.fact"), "{\"x\": 1}\n").unwrap();
        std::fs::write(dir.path().join("README"), "not a fact\n").unwrap();
        // An executable fact gets the arguments, and an empty standard input.
        let exe = dir.path().join("args.fact");
        std::fs::write(
            &exe,
            "#!/bin/sh\ncat\nprintf '{\"args\": \"%s\"}\\n' \"$*\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let pattern = format!("{}/*.fact", dir.path().display());
        let output = run_remote(
            &facts_command(&pattern, &["--json".into()]),
            b"",
            tmp.path(),
        );
        let facts = parse_facts_output(&output);
        assert_eq!(facts["owner"], serde_json::json!({"team": "a"}));
        assert_eq!(facts["args"], serde_json::json!({"args": "--json"}));
        assert!(!facts.contains_key("README"));
        // A pattern skips dotfiles, as Ansible's glob does; named outright,
        // one whose name is only an extension keeps its name.
        assert!(!facts.contains_key(".local"));
        let local = format!("{}/.local.fact", dir.path().display());
        let named = parse_facts_output(&run_remote(&facts_command(&local, &[]), b"", tmp.path()));
        assert_eq!(named[".local"], serde_json::json!({"x": 1}));
    }
}
