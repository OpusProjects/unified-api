use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::application::credentials::resolve_credentials;
use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::HostVars;
use crate::domain::source::Source;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::sync_mode::SyncMode;
use crate::ports::cache::CachePort;
use crate::ports::connector::{ConnectorOutput, ConnectorPort};
use crate::ports::secrets::SecretsPort;

// Scope of a sync: the complete inventory, a named set of hosts, or a group.
//
// Hosts is a list rather than a single name because `?host=` accepts a
// comma-separated list everywhere else in the API, and a caller refreshing the
// five hosts a form displays should pay for one gather, not five. A single-name
// list is the common case and keeps its old label.
pub enum SyncScope {
    Full,
    Hosts(Vec<String>),
    Group(String),
}

impl SyncScope {
    // Readable label for logs and responses: "full", "host:x", "host:x,y",
    // "group:y"
    pub fn label(&self) -> String {
        match self {
            SyncScope::Full => "full".to_string(),
            SyncScope::Hosts(hosts) => format!("host:{}", hosts.join(",")),
            SyncScope::Group(group) => format!("group:{}", group),
        }
    }

    // Parse the `?host=` form: comma-separated, trimmed, empties dropped.
    // Returns None when nothing survives, so the caller can fall back to
    // another scope instead of syncing an empty host set.
    pub fn hosts_from_csv(value: &str) -> Option<Self> {
        let hosts: Vec<String> = value
            .split(',')
            .map(|h| h.trim())
            .filter(|h| !h.is_empty())
            .map(|h| h.to_string())
            .collect();

        (!hosts.is_empty()).then_some(SyncScope::Hosts(hosts))
    }
}

// Result of a sync — pure data, no HTTP types.
// The handler converts it to JSON; the scheduler converts it to logs.
pub struct SyncOutcome {
    pub scope: String,
    pub total_hosts: usize,
    pub total_groups: usize,
    pub duration_ms: u128,
    pub error: Option<String>,
}

impl SyncOutcome {
    pub fn success(&self) -> bool {
        self.error.is_none()
    }

    fn failed(scope: String, duration_ms: u128, error: String) -> Self {
        Self {
            scope,
            total_hosts: 0,
            total_groups: 0,
            duration_ms,
            error: Some(error),
        }
    }
}

// The use case "sync a source": resolve credentials, execute
// the connector, and apply the result to cache based on scope and sync_mode.
//
// The caller chooses the connector (ProcessConnector or SshConnector, based on
// source.connector_type) and passes it already resolved — this way this function only
// depends on ports, not on AppState.
pub async fn sync_source(
    cache: &dyn CachePort,
    connector: &dyn ConnectorPort,
    secrets: &dyn SecretsPort,
    health: &SyncHealthRegistry,
    source_id: &str,
    source: &Source,
    scope: SyncScope,
) -> SyncOutcome {
    let outcome = run_sync(cache, connector, secrets, source_id, source, scope).await;

    // Recorded here rather than at the call sites so the scheduler and the HTTP
    // handler cannot drift: every sync in the process goes through this
    // function. Until now a failed scheduled sync left nothing behind but a log
    // line, so a stale dataset could not be told apart from a slow one.
    match &outcome.error {
        None => health.record_success(source_id),
        Some(error) => health.record_failure(source_id, error),
    }

    // One counter per outcome and a duration histogram, labeled by source.
    // The metrics facade works like tracing: recording here is fine for the
    // application layer, the exporter lives in the adapters.
    let result_label = if outcome.success() {
        "success"
    } else {
        "error"
    };
    metrics::counter!(
        "unified_api_sync_total",
        "source" => source_id.to_string(),
        "result" => result_label,
    )
    .increment(1);
    metrics::histogram!(
        "unified_api_sync_duration_seconds",
        "source" => source_id.to_string(),
    )
    .record(outcome.duration_ms as f64 / 1000.0);

    outcome
}

async fn run_sync(
    cache: &dyn CachePort,
    connector: &dyn ConnectorPort,
    secrets: &dyn SecretsPort,
    source_id: &str,
    source: &Source,
    scope: SyncScope,
) -> SyncOutcome {
    let scope_label = scope.label();

    // The scope travels to the connector script via its config
    let mut config = source.config.clone();

    // Dynamic host list: resolve against the other source's CACHED dataset
    // and hand the result to the SSH connector as a hosts_spec JSON (its
    // internal contract). Resolution happens here — the connector must not
    // depend on the cache.
    if let Some(hfs) = &source.hosts_from_source {
        let entry = match cache.get(&hfs.source) {
            Some(entry) => entry,
            None => {
                return SyncOutcome::failed(
                    scope_label,
                    0,
                    format!(
                        "hosts_from_source '{}' is not in the cache yet — sync it first",
                        hfs.source
                    ),
                );
            }
        };

        let (specs, warnings) = hfs.resolve(&entry.dataset);
        for warning in warnings {
            tracing::warn!(source = %source_id, "{}", warning);
        }
        if specs.is_empty() {
            return SyncOutcome::failed(
                scope_label,
                0,
                format!(
                    "hosts_from_source '{}' resolved to zero hosts (pattern too narrow, or empty source?)",
                    hfs.source
                ),
            );
        }

        let spec_json = match serde_json::to_string(&specs) {
            Ok(json) => json,
            Err(e) => {
                return SyncOutcome::failed(
                    scope_label,
                    0,
                    format!("failed to serialize hosts_spec: {}", e),
                );
            }
        };
        tracing::debug!(source = %source_id, hosts = specs.len(), from = %hfs.source, "Resolved dynamic host list");
        config.insert("hosts_spec".to_string(), spec_json);
    }

    match &scope {
        // `target` stays a comma-joined string: it is the shape connector
        // scripts have always received (the query value, verbatim), and a
        // single host renders identically.
        SyncScope::Hosts(hosts) => {
            config.insert("scope".to_string(), "host".to_string());
            config.insert("target".to_string(), hosts.join(","));
        }
        SyncScope::Group(group) => {
            config.insert("scope".to_string(), "group".to_string());
            config.insert("target".to_string(), group.clone());
        }
        SyncScope::Full => {}
    }

    let start = Instant::now();

    let credentials = match resolve_credentials(secrets, &source.credential_ids).await {
        Ok(creds) => creds,
        Err(e) => return SyncOutcome::failed(scope_label, start.elapsed().as_millis(), e.message),
    };

    // The timeout protects the scheduler and the API from a hung connector
    // script: without it, a stuck process blocks its sync task forever.
    let result = match timeout(
        Duration::from_secs(source.timeout_seconds),
        connector.execute(
            &source.script_path,
            &source.script_args,
            source.output_format,
            &config,
            &credentials,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => {
            return SyncOutcome::failed(
                scope_label,
                start.elapsed().as_millis(),
                format!("sync timed out after {}s", source.timeout_seconds),
            );
        }
    };

    let duration_ms = start.elapsed().as_millis();

    match result {
        Ok(output) => {
            let total_hosts = output.dataset.hostvars.len();
            let total_groups = output.dataset.groups.len();

            apply_to_cache(cache, source_id, source, &scope, output);

            SyncOutcome {
                scope: scope_label,
                total_hosts,
                total_groups,
                duration_ms,
                error: None,
            }
        }
        Err(e) => SyncOutcome::failed(scope_label, duration_ms, e.message),
    }
}

// Applies the dataset returned by the connector to the cache. All merges
// go through merge_or_insert / update: the decision "does the entry exist?" and the
// modification occur under the same lock (see CachePort).
fn apply_to_cache(
    cache: &dyn CachePort,
    source_id: &str,
    source: &Source,
    scope: &SyncScope,
    output: ConnectorOutput,
) {
    let ConnectorOutput { dataset, ages } = output;
    match scope {
        SyncScope::Hosts(hosts) => {
            // Only the requested hosts the connector actually returned. One
            // missing host does not discard the others: a batch refresh of five
            // hosts where one is unreachable still updates the four that
            // answered.
            let updates: Vec<(String, HostVars, u64)> = hosts
                .iter()
                .filter_map(|host| {
                    let vars = dataset.hostvars.get(host).cloned()?;
                    // A connector that knows how old its data is (the remote
                    // one, federating another instance's cache) backdates the
                    // host; a local gather happened now, so age zero.
                    let age = ages
                        .as_ref()
                        .and_then(|a| a.host_ages.get(host).copied())
                        .unwrap_or(0);
                    Some((host.clone(), vars, age))
                })
                .collect();

            if !updates.is_empty() {
                cache.merge_or_insert(
                    source_id,
                    dataset,
                    source.ttl_seconds,
                    &mut |entry, _new| {
                        for (hostname, vars, age) in &updates {
                            entry.update_host_aged(hostname.clone(), vars.clone(), *age);
                        }
                    },
                );

                // merge_or_insert's closure only runs when the entry already
                // existed; its insert branch builds a CacheEntry::new, which
                // stamps every host "now". On a cold central (no scheduled sync
                // yet, a consumer asking for a host) that would throw away the
                // origin's ages this whole path exists to preserve, so the
                // timestamps are corrected in a second atomic pass. Re-stamping
                // an already-correct entry is idempotent, and this only runs for
                // connectors that report ages at all.
                if ages.is_some() {
                    cache.update(source_id, &mut |entry| {
                        for (hostname, _vars, age) in &updates {
                            if let Some(vars) = entry.dataset.hostvars.get(hostname).cloned() {
                                entry.update_host_aged(hostname.clone(), vars, *age);
                            }
                        }
                    });
                }
            }
        }
        SyncScope::Group(group) => {
            cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                entry.update_group(group, new)
            });
        }
        SyncScope::Full => match source.sync_mode {
            SyncMode::Replace => {
                // A connector that reports how old its data already is (the
                // remote/federation one) gets an entry with truthful ages;
                // everything else gathered fresh and starts at age zero.
                let entry = match ages {
                    Some(a) => CacheEntry::restore(
                        dataset,
                        source.ttl_seconds,
                        a.dataset_age_seconds,
                        a.host_ages,
                    ),
                    None => CacheEntry::new(dataset, source.ttl_seconds),
                };
                cache.set(source_id, entry);
            }
            SyncMode::Merge => {
                cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                    entry.merge_dataset(new)
                });
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_host_keeps_the_old_label() {
        let scope = SyncScope::hosts_from_csv("motoko.section9.net").unwrap();
        assert_eq!(scope.label(), "host:motoko.section9.net");
    }

    #[test]
    fn several_hosts_are_listed_in_the_label() {
        let scope = SyncScope::hosts_from_csv("a.example,b.example").unwrap();
        assert_eq!(scope.label(), "host:a.example,b.example");
    }

    #[test]
    fn hosts_from_csv_trims_and_drops_empties() {
        let scope = SyncScope::hosts_from_csv(" a.example , , b.example ").unwrap();
        match scope {
            SyncScope::Hosts(hosts) => assert_eq!(hosts, vec!["a.example", "b.example"]),
            _ => panic!("expected a host scope"),
        }
    }

    #[test]
    fn a_value_with_no_hostnames_is_not_a_host_scope() {
        // the caller falls back to another scope instead of syncing nothing
        assert!(SyncScope::hosts_from_csv(" , , ").is_none());
        assert!(SyncScope::hosts_from_csv("").is_none());
    }

    #[test]
    fn the_other_labels_are_unchanged() {
        assert_eq!(SyncScope::Full.label(), "full");
        assert_eq!(SyncScope::Group("magi".to_string()).label(), "group:magi");
    }
}
