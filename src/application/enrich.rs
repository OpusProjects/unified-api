use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::domain::dataset::{Dataset, Group, HostVars};
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
    // Who caused this run — an HTTP request id, "scheduled", or the trigger of
    // the sync being re-applied. Handed to a script enricher inside
    // SOURCE_CONFIG as the reserved `trigger` key, exactly like a connector's,
    // so its logs join the same trace. A declarative merge runs no script and
    // ignores it.
    trigger: Option<&str>,
) -> Option<EnrichOutcome> {
    let result = if enricher.is_declarative() {
        execute_declarative_merge(cache, enricher)
    } else {
        execute_enricher(
            cache,
            enricher_port,
            projects_dir,
            enricher_id,
            enricher,
            trigger,
        )
        .await
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
    trigger: Option<&str>,
) -> usize {
    let mut matching: Vec<(&String, &Enricher)> = enrichers
        .iter()
        .filter(|(_, enricher)| enricher.target_id == target_id)
        .collect();
    matching.sort_by(|a, b| a.0.cmp(b.0));

    let mut applied = 0;
    for (id, enricher) in matching {
        if run_enricher(
            cache,
            enricher_port,
            health,
            projects_dir,
            id,
            enricher,
            trigger,
        )
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

    // A field may be declared on the source as a GROUP var rather than on the
    // host — the usual place for one that describes a whole tenancy, and the
    // only place for one whose group has no members in the source at all. The
    // source cannot say which hosts are in that group; the target can, so the
    // membership is read from the target and the values from the source.
    let group_vars = resolve_group_vars(&target_entry.dataset, &source_entry.dataset);

    for hostname in target_entry.dataset.hostvars.keys() {
        // Only the keys this enricher owns. Writing the whole host map
        // back — a clone of the target plus our field — is what made two
        // enrichers on one target race: each carried its own snapshot of
        // the other's keys, and whichever committed last erased the rest.
        let mut owned = HostVars::new();

        // Group vars first: a host's own vars in the source are the more
        // specific statement about it, so they are applied second and win.
        if let Some(from_groups) = group_vars.get(hostname) {
            for field in fields {
                if let Some(value) = from_groups.get(field) {
                    owned.insert(field.clone(), value.clone());
                }
            }
        }
        if let Some(source_vars) = source_entry.dataset.hostvars.get(hostname) {
            for field in fields {
                if let Some(value) = source_vars.get(field) {
                    owned.insert(field.clone(), value.clone());
                }
            }
        }

        if !owned.is_empty() {
            partial_hostvars.insert(hostname.clone(), owned);
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

// How deeply each group sits, so a host's groups can be applied in the order a
// consumer would apply them: a group nested further down is the more specific
// statement about its members, and its value should land last.
//
// Relaxed rather than walked, and bounded by the number of groups: a group tree
// is really a graph — the same group may be declared under several parents —
// and the bound terminates even if one ends up naming itself.
fn group_depths(groups: &HashMap<String, Group>) -> HashMap<String, usize> {
    let mut depth: HashMap<String, usize> = groups.keys().map(|n| (n.clone(), 0)).collect();

    for _ in 0..groups.len() {
        let mut changed = false;
        for (name, group) in groups {
            let d = depth.get(name).copied().unwrap_or(0);
            for child in &group.children {
                let entry = depth.entry(child.clone()).or_insert(0);
                if *entry < d + 1 {
                    *entry = d + 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    depth
}

// The source's group vars, resolved onto the target's hosts by group NAME.
//
// Membership comes from the target and values from the source, which is what
// lets a source describe a group it has no members of. Only variables cross —
// never hosts — so a source cannot pull its own hosts into the target this way.
//
// Ancestry counts: a host in a child group would inherit from every parent when
// the inventory is resolved, so resolving only direct membership here would give
// the host a different answer than the inventory it is enriching.
fn resolve_group_vars(target: &Dataset, source: &Dataset) -> HashMap<String, HostVars> {
    let mut per_host: HashMap<String, HostVars> = HashMap::new();
    if source.groups.values().all(|g| g.vars.is_none()) {
        return per_host;
    }

    let depth = group_depths(&target.groups);

    // child -> parents, so a group can be walked up to every ancestor of it.
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, group) in &target.groups {
        for child in &group.children {
            parents
                .entry(child.as_str())
                .or_default()
                .push(name.as_str());
        }
    }

    // Each group's effective vars = its own, under every ancestor's. Computed
    // per group rather than per host: every host in a group gets the same set.
    let mut effective: HashMap<&str, HostVars> = HashMap::new();
    for name in target.groups.keys() {
        let mut chain: Vec<(usize, &str)> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut pending: Vec<&str> = vec![name.as_str()];
        while let Some(group) = pending.pop() {
            if !seen.insert(group) {
                continue;
            }
            chain.push((depth.get(group).copied().unwrap_or(0), group));
            if let Some(above) = parents.get(group) {
                pending.extend(above.iter().copied());
            }
        }
        chain.sort();

        let mut merged = HostVars::new();
        for (_, group) in &chain {
            if let Some(vars) = source.groups.get(*group).and_then(|g| g.vars.as_ref()) {
                merged.extend(vars.clone());
            }
        }
        if !merged.is_empty() {
            effective.insert(name.as_str(), merged);
        }
    }

    // Applied shallowest first, so a host in two groups takes the deeper one's
    // value — and alphabetically within a depth, so the answer is the same on
    // every run rather than whatever order the map happened to iterate in.
    let mut ordered: Vec<(usize, &str)> = target
        .groups
        .keys()
        .map(|n| (depth.get(n).copied().unwrap_or(0), n.as_str()))
        .collect();
    ordered.sort();

    for (_, name) in &ordered {
        let (Some(vars), Some(group)) = (effective.get(name), target.groups.get(*name)) else {
            continue;
        };
        for host in &group.hosts {
            per_host
                .entry(host.clone())
                .or_default()
                .extend(vars.clone());
        }
    }

    per_host
}

async fn execute_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    projects_dir: &std::path::Path,
    enricher_id: &str,
    enricher: &Enricher,
    trigger: Option<&str>,
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
    if let Some(trigger) = trigger {
        config.insert("trigger".to_string(), trigger.to_string());
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
            None,
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
            None,
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
            None,
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
                None,
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
            None,
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

    fn group(hosts: &[&str], children: &[&str], vars: &[(&str, &str)]) -> Group {
        Group {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            children: children.iter().map(|c| c.to_string()).collect(),
            vars: if vars.is_empty() {
                None
            } else {
                Some(
                    vars.iter()
                        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
                        .collect(),
                )
            },
        }
    }

    // The field describes a whole tenancy, so it is declared once on the group
    // rather than on each host — and the source has no members of that group at
    // all. Only the target knows who is in it, which is the point of resolving
    // the source's group vars through the target's membership.
    #[tokio::test]
    async fn a_declarative_merge_resolves_the_sources_group_vars() {
        let cache = MemoryCache::new();
        cache.set(
            "src-a",
            CacheEntry::new(
                Dataset {
                    groups: [(
                        "section9".to_string(),
                        group(&["motoko.section9.net"], &[], &[]),
                    )]
                    .into_iter()
                    .collect(),
                    ..dataset()
                },
                3600,
            ),
        );
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: HashMap::new(),
                    groups: [(
                        "section9".to_string(),
                        group(
                            &[],
                            &[],
                            &[("infinibox", "vol-group"), ("unrelated", "nope")],
                        ),
                    )]
                    .into_iter()
                    .collect(),
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
            None,
        )
        .await
        .expect("target is cached");

        assert!(outcome.success());
        assert_eq!(outcome.hosts_updated, 1);
        let entry = cache.get("src-a").expect("entry");
        let host = &entry.dataset.hostvars["motoko.section9.net"];
        assert_eq!(host["infinibox"], "vol-group");
        assert!(
            !host.contains_key("unrelated"),
            "the allow-list still applies to a group's vars"
        );
        // No host crossed over: only variables travel.
        assert_eq!(entry.dataset.hostvars.len(), 1);
    }

    // The host's own entry in the source is the more specific statement about
    // it, so it is applied after the group's and wins.
    #[tokio::test]
    async fn a_hosts_own_source_vars_beat_the_groups() {
        let cache = MemoryCache::new();
        cache.set(
            "src-a",
            CacheEntry::new(
                Dataset {
                    groups: [(
                        "section9".to_string(),
                        group(&["motoko.section9.net"], &[], &[]),
                    )]
                    .into_iter()
                    .collect(),
                    ..dataset()
                },
                3600,
            ),
        );
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: [(
                        "motoko.section9.net".to_string(),
                        [("infinibox".to_string(), serde_json::json!("vol-host"))]
                            .into_iter()
                            .collect(),
                    )]
                    .into_iter()
                    .collect(),
                    groups: [(
                        "section9".to_string(),
                        group(&[], &[], &[("infinibox", "vol-group")]),
                    )]
                    .into_iter()
                    .collect(),
                    remove_hosts: Vec::new(),
                },
                3600,
            ),
        );

        run_enricher(
            &cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-d",
            &declarative_enricher(),
            None,
        )
        .await
        .expect("target is cached");

        let entry = cache.get("src-a").expect("entry");
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["infinibox"],
            "vol-host"
        );
    }

    // A host in a child group inherits from every ancestor of it, and the
    // deeper group is the more specific statement — the order a consumer
    // resolving the inventory itself would apply.
    #[tokio::test]
    async fn a_deeper_group_beats_the_ancestor_it_is_declared_under() {
        let cache = MemoryCache::new();
        cache.set(
            "src-a",
            CacheEntry::new(
                Dataset {
                    groups: [
                        ("section9".to_string(), group(&[], &["field"], &[])),
                        (
                            "field".to_string(),
                            group(&["motoko.section9.net"], &[], &[]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    ..dataset()
                },
                3600,
            ),
        );
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: HashMap::new(),
                    groups: [
                        (
                            "section9".to_string(),
                            group(&[], &[], &[("infinibox", "vol-parent"), ("site", "hq")]),
                        ),
                        (
                            "field".to_string(),
                            group(&[], &[], &[("infinibox", "vol-child")]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    remove_hosts: Vec::new(),
                },
                3600,
            ),
        );

        let enricher: Enricher = serde_yaml_ng::from_str(
            "name: d\ntarget_id: src-a\nsource_id: src-b\nfields: [\"infinibox\", \"site\"]\n",
        )
        .expect("enricher fixture");

        run_enricher(
            &cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-d",
            &enricher,
            None,
        )
        .await
        .expect("target is cached");

        let entry = cache.get("src-a").expect("entry");
        let host = &entry.dataset.hostvars["motoko.section9.net"];
        // the child's value wins where both declare it
        assert_eq!(host["infinibox"], "vol-child");
        // and the ancestor's own field still reaches the host
        assert_eq!(host["site"], "hq");
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .await
        .expect("target is cached now");

        let recorded = health.get("en-spy").unwrap();
        assert_eq!(recorded.consecutive_failures, 0);
        assert_eq!(recorded.last_error, None);
    }
}
