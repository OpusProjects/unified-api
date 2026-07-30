use std::collections::HashMap;
use std::sync::Arc;
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
            // An Arc clone: the enricher reads the very dataset the cache holds
            Arc::clone(&current_entry.dataset),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::cache_entry::CacheEntry;
    use crate::ports::enricher::{EnricherError, EnricherResult};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    // Keeps the dataset it was handed, so a test can prove it is the very one
    // the cache holds rather than a copy that happens to be equal.
    #[derive(Default)]
    struct SpyEnricher {
        received: Mutex<Option<Arc<Dataset>>>,
        fail: bool,
    }

    impl EnricherPort for SpyEnricher {
        fn execute(
            &self,
            _script_path: &str,
            _args: &[String],
            _config: &HashMap<String, String>,
            current_dataset: Arc<Dataset>,
        ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>> {
            *self.received.lock().expect("spy lock") = Some(Arc::clone(&current_dataset));
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    return Err(EnricherError {
                        message: "spy failure".to_string(),
                    });
                }
                Ok(Dataset {
                    hostvars: HashMap::new(),
                    groups: HashMap::new(),
                    remove_hosts: Vec::new(),
                })
            })
        }
    }

    fn dataset() -> Dataset {
        Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("commander"))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: Vec::new(),
        }
    }

    fn script_enricher() -> Enricher {
        serde_yaml_ng::from_str("name: spy\ntarget_id: src-a\nscript_path: /bin/true\n")
            .expect("enricher fixture")
    }

    // The property, not the implementation: whatever the adapter does with it,
    // the enricher must be handed the cache's own dataset. A deep copy of a
    // facts source is megabytes per run, per enricher, per interval.
    #[tokio::test]
    async fn the_enricher_is_handed_the_dataset_the_cache_holds() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        let cached = cache.get("src-a").expect("entry").dataset;

        let spy = SpyEnricher::default();
        run_enricher(&cache, &spy, &script_enricher())
            .await
            .expect("target is cached, so the enricher runs");

        let received = spy
            .received
            .lock()
            .expect("spy lock")
            .clone()
            .expect("the enricher was called");

        assert!(
            Arc::ptr_eq(&received, &cached),
            "the enricher was handed a copy of the dataset instead of the cached one"
        );
    }

    #[tokio::test]
    async fn a_failing_enricher_reports_the_reason() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));

        let spy = SpyEnricher {
            fail: true,
            ..Default::default()
        };
        let outcome = run_enricher(&cache, &spy, &script_enricher())
            .await
            .expect("target is cached");

        assert!(!outcome.success());
        assert_eq!(outcome.error.as_deref(), Some("spy failure"));
    }

    #[tokio::test]
    async fn an_uncached_target_does_not_run_the_enricher() {
        let cache = MemoryCache::new();
        let spy = SpyEnricher::default();

        assert!(
            run_enricher(&cache, &spy, &script_enricher())
                .await
                .is_none()
        );
        assert!(spy.received.lock().expect("spy lock").is_none());
    }
}
