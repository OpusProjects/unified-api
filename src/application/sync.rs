use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::application::credentials::resolve_credentials;
use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;
use crate::domain::source::Source;
use crate::domain::sync_mode::SyncMode;
use crate::ports::cache::CachePort;
use crate::ports::connector::ConnectorPort;
use crate::ports::secrets::SecretsPort;

// Scope of a sync: the complete inventory, a single host, or a group
pub enum SyncScope {
    Full,
    Host(String),
    Group(String),
}

impl SyncScope {
    // Readable label for logs and responses: "full", "host:x", "group:y"
    pub fn label(&self) -> String {
        match self {
            SyncScope::Full => "full".to_string(),
            SyncScope::Host(host) => format!("host:{}", host),
            SyncScope::Group(group) => format!("group:{}", group),
        }
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
    source_id: &str,
    source: &Source,
    scope: SyncScope,
) -> SyncOutcome {
    let outcome = run_sync(cache, connector, secrets, source_id, source, scope).await;

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
    match &scope {
        SyncScope::Host(host) => {
            config.insert("scope".to_string(), "host".to_string());
            config.insert("target".to_string(), host.clone());
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
        connector.execute(&source.script_path, &config, &credentials),
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
        Ok(dataset) => {
            let total_hosts = dataset.hostvars.len();
            let total_groups = dataset.groups.len();

            apply_to_cache(cache, source_id, source, &scope, dataset);

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
    dataset: Dataset,
) {
    match scope {
        SyncScope::Host(host) => {
            // Only cache if the connector returned the requested host
            if let Some(vars) = dataset.hostvars.get(host).cloned() {
                let hostname = host.clone();
                cache.merge_or_insert(
                    source_id,
                    dataset,
                    source.ttl_seconds,
                    &mut |entry, _new| entry.update_host(hostname.clone(), vars.clone()),
                );
            }
        }
        SyncScope::Group(group) => {
            cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                entry.update_group(group, new)
            });
        }
        SyncScope::Full => match source.sync_mode {
            SyncMode::Replace => {
                cache.set(source_id, CacheEntry::new(dataset, source.ttl_seconds));
            }
            SyncMode::Merge => {
                cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                    entry.merge_dataset(new)
                });
            }
        },
    }
}
