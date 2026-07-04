use std::time::{Duration, Instant};

use tokio::time::timeout;

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
// Returns None if the source is not in cache — there is nothing to enrich.
pub async fn run_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    let outcome = execute_enricher(cache, enricher_port, enricher).await?;

    let result_label = if outcome.success() {
        "success"
    } else {
        "error"
    };
    metrics::counter!(
        "unified_api_enrich_total",
        "source" => enricher.source_id.clone(),
        "result" => result_label,
    )
    .increment(1);
    metrics::histogram!(
        "unified_api_enrich_duration_seconds",
        "source" => enricher.source_id.clone(),
    )
    .record(outcome.duration_ms as f64 / 1000.0);

    Some(outcome)
}

async fn execute_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    // Read snapshot: the enricher runs a script and takes time, we cannot
    // hold the cache lock while it runs. The merge below is atomic, so
    // concurrent writes during execution are not lost (the enricher only
    // overwrites the hosts that it itself returns).
    let current_entry = cache.get(&enricher.source_id)?;

    let start = Instant::now();

    // Same rationale as in sync: a hung enricher script must not block
    // its scheduler task forever.
    let result = match timeout(
        Duration::from_secs(enricher.timeout_seconds),
        enricher_port.execute(
            &enricher.script_path,
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

            // Option::take inside the closure: merge_dataset consumes the
            // dataset but a FnMut cannot move what it captures
            let mut partial = Some(partial_dataset);
            cache.update(&enricher.source_id, &mut |entry| {
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
