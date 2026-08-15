use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::domain::dataset::{Dataset, HostVars};
use crate::domain::enricher::Enricher;
use crate::domain::sync_health::SyncHealthRegistry;
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
// That case is still recorded as a failure in the health registry: an enricher
// whose target never syncs is exactly as broken as one whose script fails,
// and until now both were visible only in the logs.
//
// Health is recorded here rather than at the call sites so the scheduler, the
// HTTP handler and the post-sync re-apply cannot drift — the same reasoning
// as sync_source recording sync health.
pub async fn run_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    health: &SyncHealthRegistry,
    // Where project checkouts live: a script enricher's path resolves into
    // its checkout at execution time (see application::scripts)
    projects_dir: &std::path::Path,
    enricher_id: &str,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    let result = if enricher.is_declarative() {
        execute_declarative_merge(cache, enricher)
    } else {
        execute_enricher(cache, enricher_port, projects_dir, enricher_id, enricher).await
    };

    let Some(outcome) = result else {
        health.record_failure(
            enricher_id,
            &format!("target '{}' is not in the cache", enricher.target_id),
        );
        return None;
    };

    match &outcome.error {
        None => health.record_success(enricher_id),
        Some(error) => health.record_failure(enricher_id, error),
    }

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
    health: &SyncHealthRegistry,
    projects_dir: &std::path::Path,
    enrichers: &HashMap<String, Enricher>,
    target_id: &str,
) -> usize {
    let mut matching: Vec<(&String, &Enricher)> = enrichers
        .iter()
        .filter(|(_, enricher)| enricher.target_id == target_id)
        .collect();
    matching.sort_by(|a, b| a.0.cmp(b.0));

    let mut applied = 0;
    for (id, enricher) in matching {
        if run_enricher(cache, enricher_port, health, projects_dir, id, enricher)
            .await
            .is_some()
        {
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
    projects_dir: &std::path::Path,
    enricher_id: &str,
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

    // An enricher that names a project runs the script from its checkout;
    // resolved per execution so a checkout that appears after boot is picked
    // up by the next run (see application::scripts)
    let script_path = match &enricher.project_id {
        Some(project_id) => crate::application::scripts::resolve_script_path(
            projects_dir,
            enricher_id,
            project_id,
            script_path,
        ),
        None => script_path.clone(),
    };

    // The project's virtualenv rides the same reserved-config channel as it
    // does for connectors; the process adapter prepends it to PATH
    let mut config = enricher.config.clone();
    if let Some(project_id) = &enricher.project_id
        && let Some(bin) = crate::application::scripts::venv_bin_dir(projects_dir, project_id)
    {
        config.insert(crate::ports::venv::VENV_BIN_CONFIG_KEY.to_string(), bin);
    }

    let start = Instant::now();

    let result = match timeout(
        Duration::from_secs(enricher.timeout_seconds),
        enricher_port.execute(
            &script_path,
            &enricher.script_args,
            &config,
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
                    // Not merge_dataset: that one is for data a connector
                    // gathered, and stamps the hosts it carries as collected
                    // now. An enricher gathers nothing.
                    entry.merge_enrichment(p);
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
        returns: Option<Dataset>,
        delay: Option<Duration>,
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
            let returns = self.returns.clone();
            let delay = self.delay;
            Box::pin(async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                if fail {
                    return Err(EnricherError {
                        message: "spy failure".to_string(),
                    });
                }
                Ok(returns.unwrap_or(Dataset {
                    hostvars: HashMap::new(),
                    groups: HashMap::new(),
                    remove_hosts: Vec::new(),
                }))
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
        run_enricher(
            &cache,
            &spy,
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-spy",
            &script_enricher(),
        )
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

    // The bug, end to end: enriching a host made it look freshly gathered, so
    // the on-demand refresh a consumer asked for found nothing stale and did
    // nothing. 0.10.0 fixed this for declarative enrichers only.
    #[tokio::test]
    async fn a_script_enricher_does_not_reset_how_fresh_a_host_looks() {
        let cache = MemoryCache::new();
        let mut entry = CacheEntry::new(dataset(), 3600);
        entry.update_host_aged(
            "motoko.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("commander"))]
                .into_iter()
                .collect(),
            900,
        );
        cache.set("src-a", entry);

        let spy = SpyEnricher {
            returns: Some(Dataset {
                hostvars: [(
                    "motoko.section9.net".to_string(),
                    [
                        ("role".to_string(), serde_json::json!("commander")),
                        ("infinibox".to_string(), serde_json::json!("vol-a")),
                    ]
                    .into_iter()
                    .collect(),
                )]
                .into_iter()
                .collect(),
                groups: HashMap::new(),
                remove_hosts: Vec::new(),
            }),
            ..Default::default()
        };

        let outcome = run_enricher(
            &cache,
            &spy,
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-spy",
            &script_enricher(),
        )
        .await
        .expect("target is cached");
        assert!(outcome.success());

        let entry = cache.get("src-a").expect("entry");
        // The derived key landed...
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["infinibox"],
            "vol-a"
        );
        // ...without the host claiming to have just been gathered, so a read
        // that asked for a refresh still gets one
        assert!(
            entry.host_age_seconds("motoko.section9.net").unwrap_or(0) >= 900,
            "enrichment reset the host's gathered-at timestamp"
        );
        assert!(!entry.is_host_fresh("motoko.section9.net", Some(600)));
    }

    #[tokio::test]
    async fn a_failing_enricher_reports_the_reason() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));

        let spy = SpyEnricher {
            fail: true,
            ..Default::default()
        };
        let health = SyncHealthRegistry::new();
        let outcome = run_enricher(
            &cache,
            &spy,
            &health,
            std::path::Path::new("unused"),
            "en-spy",
            &script_enricher(),
        )
        .await
        .expect("target is cached");

        assert!(!outcome.success());
        assert_eq!(outcome.error.as_deref(), Some("spy failure"));

        // ...and the reason lands in the health registry, where /enrichers
        // and /metrics can see it — not only in a log line
        let recorded = health.get("en-spy").expect("failure must be recorded");
        assert_eq!(recorded.last_error.as_deref(), Some("spy failure"));
        assert_eq!(recorded.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn an_uncached_target_does_not_run_the_enricher() {
        let cache = MemoryCache::new();
        let spy = SpyEnricher::default();
        let health = SyncHealthRegistry::new();

        assert!(
            run_enricher(
                &cache,
                &spy,
                &health,
                std::path::Path::new("unused"),
                "en-spy",
                &script_enricher(),
            )
            .await
            .is_none()
        );
        assert!(spy.received.lock().expect("spy lock").is_none());

        // Not running IS the failure mode worth surfacing: an enricher whose
        // target never syncs used to be a warn! on every tick and nothing else
        let recorded = health.get("en-spy").expect("skip must be recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("not in the cache"))
        );
    }

    fn declarative_enricher() -> Enricher {
        serde_yaml_ng::from_str(
            "name: d\ntarget_id: src-a\nsource_id: src-b\nfields: [\"infinibox\"]\n",
        )
        .expect("enricher fixture")
    }

    #[tokio::test]
    async fn a_declarative_merge_copies_only_the_named_fields() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: [(
                        "motoko.section9.net".to_string(),
                        [
                            ("infinibox".to_string(), serde_json::json!("vol-a")),
                            ("unrelated".to_string(), serde_json::json!("nope")),
                        ]
                        .into_iter()
                        .collect(),
                    )]
                    .into_iter()
                    .collect(),
                    groups: HashMap::new(),
                    remove_hosts: Vec::new(),
                },
                3600,
            ),
        );

        let outcome = run_enricher(
            &cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-d",
            &declarative_enricher(),
        )
        .await
        .expect("target is cached");

        assert!(outcome.success());
        assert_eq!(outcome.hosts_updated, 1);
        let entry = cache.get("src-a").expect("entry");
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["infinibox"],
            "vol-a"
        );
        assert!(
            !entry.dataset.hostvars["motoko.section9.net"].contains_key("unrelated"),
            "only the declared fields may be copied"
        );
    }

    #[tokio::test]
    async fn a_declarative_merge_with_an_uncached_source_reports_it() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        // src-b is not in the cache

        let outcome = run_enricher(
            &cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-d",
            &declarative_enricher(),
        )
        .await
        .expect("target IS cached, so an outcome is produced");

        assert!(!outcome.success());
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("src-b")),
            "error was: {:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn an_enricher_with_neither_mode_reports_the_missing_script() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));

        // Config validation refuses this at startup, but the domain type can
        // exist: the use case must answer with words, not a panic
        let enricher: Enricher =
            serde_yaml_ng::from_str("name: bare\ntarget_id: src-a\n").expect("fixture");
        let outcome = run_enricher(
            &cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-bare",
            &enricher,
        )
        .await
        .expect("target is cached");

        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("script_path")),
            "error was: {:?}",
            outcome.error
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_enricher_times_out_and_records_the_failure() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));

        let spy = SpyEnricher {
            delay: Some(Duration::from_secs(3600)),
            ..Default::default()
        };
        // project_id exercises the checkout resolution branch on the way in
        let enricher: Enricher = serde_yaml_ng::from_str(
            "name: hung\ntarget_id: src-a\nscript_path: /bin/true\nproject_id: prj-x\ntimeout_seconds: 5\n",
        )
        .expect("fixture");

        let health = SyncHealthRegistry::new();
        let outcome = run_enricher(
            &cache,
            &spy,
            &health,
            std::path::Path::new("unused"),
            "en-hung",
            &enricher,
        )
        .await
        .expect("target is cached");

        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 5s")),
            "error was: {:?}",
            outcome.error
        );
        assert_eq!(health.get("en-hung").unwrap().consecutive_failures, 1);
    }

    #[tokio::test]
    async fn a_success_clears_an_earlier_failure() {
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();

        // First run: target missing, recorded as a failure
        let spy = SpyEnricher::default();
        run_enricher(
            &cache,
            &spy,
            &health,
            std::path::Path::new("unused"),
            "en-spy",
            &script_enricher(),
        )
        .await;
        assert_eq!(health.get("en-spy").unwrap().consecutive_failures, 1);

        // Target appears, the enricher runs: healthy again
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        run_enricher(
            &cache,
            &spy,
            &health,
            std::path::Path::new("unused"),
            "en-spy",
            &script_enricher(),
        )
        .await
        .expect("target is cached now");

        let recorded = health.get("en-spy").unwrap();
        assert_eq!(recorded.consecutive_failures, 0);
        assert_eq!(recorded.last_error, None);
    }
}
