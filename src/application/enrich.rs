use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::domain::dataset::{Dataset, HostVars};
use crate::domain::enricher::Enricher;
use crate::ports::cache::CachePort;
use crate::ports::enricher::EnricherPort;

// Result of running an enricher — pure data, no HTTP types
pub struct EnrichOutcome {
    pub hosts_updated: usize,
    pub hosts_removed: usize,
    pub duration_ms: u128,
    pub error: Option<String>,
}

impl EnrichOutcome {
    pub fn success(&self) -> bool {
        self.error.is_none()
    }
}

// The use case "enrich a source": execute the enricher script
// against the cached dataset and merge the partial result.
//
// Returns None if the target is not in cache — there is nothing to enrich.
pub async fn run_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    let outcome = if enricher.is_declarative() {
        execute_declarative_merge(cache, enricher)?
    } else {
        execute_enricher(cache, enricher_port, enricher).await?
    };

    let result_label = if outcome.success() {
        "success"
    } else {
        "error"
    };
    metrics::counter!(
        "unified_api_enrich_total",
        "source" => enricher.target_id.clone(),
        "result" => result_label,
    )
    .increment(1);
    metrics::histogram!(
        "unified_api_enrich_duration_seconds",
        "source" => enricher.target_id.clone(),
    )
    .record(outcome.duration_ms as f64 / 1000.0);

    Some(outcome)
}

// Every enricher that targets `target_id`, applied in a stable order.
//
// Sorted by id and run one after another: additive merges cannot lose each
// other's keys, but if two enrichers ever claim the *same* key on the same
// host the winner should be a documented rule rather than whichever task
// happened to finish last. Same reasoning as a view's member order.
pub async fn run_enrichers_for_target(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enrichers: &HashMap<String, Enricher>,
    target_id: &str,
) -> usize {
    let mut matching: Vec<(&String, &Enricher)> = enrichers
        .iter()
        .filter(|(_, enricher)| enricher.target_id == target_id)
        .collect();
    matching.sort_by(|a, b| a.0.cmp(b.0));

    let mut applied = 0;
    for (_, enricher) in matching {
        if run_enricher(cache, enricher_port, enricher).await.is_some() {
            applied += 1;
        }
    }
    applied
}

fn execute_declarative_merge(cache: &dyn CachePort, enricher: &Enricher) -> Option<EnrichOutcome> {
    let start = Instant::now();

    let source_id = match &enricher.source_id {
        Some(id) => id,
        None => {
            return Some(EnrichOutcome {
                hosts_updated: 0,
                hosts_removed: 0,
                duration_ms: start.elapsed().as_millis(),
                error: Some("declarative enricher missing source_id".to_string()),
            });
        }
    };

    let target_entry = cache.get(&enricher.target_id)?;
    let source_entry = match cache.get(source_id) {
        Some(e) => e,
        None => {
            return Some(EnrichOutcome {
                hosts_updated: 0,
                hosts_removed: 0,
                duration_ms: start.elapsed().as_millis(),
                error: Some(format!("source '{}' not in cache", source_id)),
            });
        }
    };

    let fields = enricher.fields.as_deref().unwrap_or(&[]);
    let mut partial_hostvars = std::collections::HashMap::new();

    for hostname in target_entry.dataset.hostvars.keys() {
        if let Some(source_vars) = source_entry.dataset.hostvars.get(hostname) {
            // Only the keys this enricher owns. Writing the whole host map
            // back — a clone of the target plus our field — is what made two
            // enrichers on one target race: each carried its own snapshot of
            // the other's keys, and whichever committed last erased the rest.
            let mut owned = HostVars::new();

            for field in fields {
                if let Some(value) = source_vars.get(field) {
                    owned.insert(field.clone(), value.clone());
                }
            }

            if !owned.is_empty() {
                partial_hostvars.insert(hostname.clone(), owned);
            }
        }
    }

    let hosts_updated = partial_hostvars.len();

    let partial = Dataset {
        hostvars: partial_hostvars,
        groups: std::collections::HashMap::new(),
        remove_hosts: Vec::new(),
    };

    let mut partial = Some(partial);
    cache.update(&enricher.target_id, &mut |entry| {
        if let Some(p) = partial.take() {
            entry.merge_hostvar_fields(p);
        }
    });

    Some(EnrichOutcome {
        hosts_updated,
        hosts_removed: 0,
        duration_ms: start.elapsed().as_millis(),
        error: None,
    })
}

async fn execute_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    let current_entry = cache.get(&enricher.target_id)?;

    let script_path = match &enricher.script_path {
        Some(p) => p,
        None => {
            return Some(EnrichOutcome {
                hosts_updated: 0,
                hosts_removed: 0,
                duration_ms: 0,
                error: Some("script-based enricher missing script_path".to_string()),
            });
        }
    };

    let start = Instant::now();

    let result = match timeout(
        Duration::from_secs(enricher.timeout_seconds),
        enricher_port.execute(
            script_path,
            &enricher.script_args,
            &enricher.config,
            &current_entry.dataset,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => {
            return Some(EnrichOutcome {
                hosts_updated: 0,
                hosts_removed: 0,
                duration_ms: start.elapsed().as_millis(),
                error: Some(format!(
                    "enricher timed out after {}s",
                    enricher.timeout_seconds
                )),
            });
        }
    };

    let duration_ms = start.elapsed().as_millis();

    Some(match result {
        Ok(partial_dataset) => {
            let hosts_updated = partial_dataset.hostvars.len();
            let hosts_removed = partial_dataset.remove_hosts.len();

            let mut partial = Some(partial_dataset);
            cache.update(&enricher.target_id, &mut |entry| {
                if let Some(p) = partial.take() {
                    entry.merge_dataset(p);
                }
            });

            EnrichOutcome {
                hosts_updated,
                hosts_removed,
                duration_ms,
                error: None,
            }
        }
        Err(e) => EnrichOutcome {
            hosts_updated: 0,
            hosts_removed: 0,
            duration_ms,
            error: Some(e.message),
        },
    })
}
