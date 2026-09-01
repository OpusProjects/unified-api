use std::collections::HashMap;
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

    // None means every var the source declares, which is how Ansible itself
    // treats a group's vars: membership carries all of them, with no per-name
    // permission. `fields` is the narrowing, not the default. It used to mean
    // the opposite -- an enricher with no `fields` copied nothing and reported
    // success, a config that looked active and was not.
    let fields = enricher.fields.as_deref();
    let mut partial_hostvars = std::collections::HashMap::new();

    // A field may be declared on the source as a GROUP var rather than on the
    // host — the usual place for one that describes a whole tenancy, and the
    // only place for one whose group has no members in the source at all. The
    // source cannot say which hosts are in that group; the target can, so the
    // membership is read from the target and the values from the source.
    // A host's own vars in the source are the only thing copied per host. What
    // the source says about a GROUP is carried onto the group instead, below:
    // resolving it here would write one copy per member, which for a group of
    // 780 is the duplication 0.25.0 took out of the static-inventory connector.
    for hostname in target_entry.dataset.hostvars.keys() {
        let Some(source_vars) = source_entry.dataset.hostvars.get(hostname) else {
            continue;
        };

        // Only the keys this enricher owns. Writing the whole host map
        // back — a clone of the target plus our field — is what made two
        // enrichers on one target race: each carried its own snapshot of
        // the other's keys, and whichever committed last erased the rest.
        // Without `fields` the keys it owns are the source's, all of them,
        // which is still a subset of the target's map rather than a snapshot.
        let owned = selected(source_vars, fields, enricher.fields_excluded.as_deref());

        if !owned.is_empty() {
            partial_hostvars.insert(hostname.clone(), owned);
        }
    }

    let hosts_updated = partial_hostvars.len();

    // The source's group vars, carried onto the groups the target already has
    // rather than resolved onto each of its hosts. A group's value is then
    // stored once instead of once per member, and the consumer resolves it --
    // the same trade the static-inventory connector makes.
    let partial_groups =
        group_vars_for_target(&target_entry.dataset, &source_entry.dataset, enricher);

    let partial = Dataset {
        hostvars: partial_hostvars,
        groups: partial_groups,
        remove_hosts: Vec::new(),
    };

    let mut partial = Some(partial);
    cache.update(&enricher.target_id, &mut |entry| {
        if let Some(p) = partial.take() {
            entry.merge_group_var_fields(Dataset {
                hostvars: HashMap::new(),
                groups: p.groups,
                remove_hosts: Vec::new(),
            });
            entry.merge_hostvar_fields(Dataset {
                hostvars: p.hostvars,
                groups: HashMap::new(),
                remove_hosts: Vec::new(),
            });
        }
    });

    Some(EnrichOutcome {
        hosts_updated,
        hosts_removed: 0,
        duration_ms: start.elapsed().as_millis(),
        error: None,
    })
}

// What the source says about the TARGET's groups, ready to merge onto them.
//
// Matched by group NAME: the source declares what a group means, the target
// decides who is in it. Only variables ever cross -- no host moves between
// sources, which is what makes an enricher safe where widening an endpoint's
// `source_ids` would not be.
//
// `all` needs no match, because in Ansible it means every host: a source's
// `all` vars apply to the whole target. It is emitted carrying the target's
// hostnames, because an endpoint drops a group with neither hosts nor children
// when it renders -- vars alone do not keep a group alive. That list is read
// only when the group is created; see merge_group_var_fields.
// One rule, applied on both axes: an absent list selects everything, a present
// one selects only what it names, and an exclusion beats an inclusion.
//
// Absent means everything because that is what a group's vars mean in Ansible --
// being in the group carries all of them, with no per-name permission.
fn admits(name: &str, allowed: Option<&[String]>, excluded: Option<&[String]>) -> bool {
    if excluded.is_some_and(|names| names.iter().any(|n| n == name)) {
        return false;
    }
    allowed.is_none_or(|names| names.iter().any(|n| n == name))
}

// The vars an enricher takes from one map, under that rule.
fn selected(vars: &HostVars, fields: Option<&[String]>, excluded: Option<&[String]>) -> HostVars {
    if fields.is_none() && excluded.is_none() {
        return vars.clone();
    }
    let mut wanted = HostVars::new();
    for (key, value) in vars {
        if admits(key, fields, excluded) {
            wanted.insert(key.clone(), value.clone());
        }
    }
    wanted
}

fn group_vars_for_target(
    target: &Dataset,
    source: &Dataset,
    enricher: &Enricher,
) -> HashMap<String, Group> {
    let fields = enricher.fields.as_deref();
    let fields_excluded = enricher.fields_excluded.as_deref();
    let groups = enricher.groups.as_deref();
    let groups_excluded = enricher.groups_excluded.as_deref();
    let mut partial: HashMap<String, Group> = HashMap::new();

    for (name, group) in &source.groups {
        let Some(vars) = &group.vars else {
            continue;
        };
        // `all` is exempt from needing a match in the target -- in Ansible it
        // means every host -- but not from being selected against. An explicit
        // list is the whole list, `all` included, so what is written is what is
        // taken.
        if !admits(name, groups, groups_excluded) {
            continue;
        }
        // A group the target does not have yet is CREATED, not skipped. The
        // source declares what a group means; who is in it may be decided later
        // and elsewhere -- by Device42 on the next sync, or by `group_by` at
        // play time from a fact the machine reports. Skipping it lost every
        // variable declared for a group whose membership is not the source's to
        // know, which is most of them.
        let is_all = name == "all";

        let wanted = selected(vars, fields, fields_excluded);
        if wanted.is_empty() {
            continue;
        }

        partial.insert(
            name.clone(),
            Group {
                hosts: if is_all {
                    target.hostvars.keys().cloned().collect()
                } else {
                    Vec::new()
                },
                children: Vec::new(),
                vars: Some(wanted),
            },
        );
    }

    partial
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
    // rather than on each host -- and the source has no members of that group at
    // all. It is carried onto the TARGET's group of the same name, where one
    // copy serves every member, instead of being written onto each of them.
    #[tokio::test]
    async fn a_declarative_merge_carries_group_vars_onto_the_group() {
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
        let vars = entry.dataset.groups["section9"]
            .vars
            .as_ref()
            .expect("the group carries the vars");
        assert_eq!(vars["infinibox"], "vol-group");
        assert!(
            !vars.contains_key("unrelated"),
            "the allow-list still applies to a group's vars"
        );
        // Not copied onto the member: that is the duplication this avoids.
        assert!(!entry.dataset.hostvars["motoko.section9.net"].contains_key("infinibox"));
        // No host crossed over: only variables travel.
        assert_eq!(entry.dataset.hostvars.len(), 1);
    }

    // `all` means every host, so it needs no matching group in the target. It is
    // created carrying the target's hosts, because an endpoint drops a group with
    // neither hosts nor children -- vars alone would be silently thrown away.
    #[tokio::test]
    async fn the_sources_all_group_is_merged_and_carries_the_targets_hosts() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: HashMap::new(),
                    groups: [(
                        "all".to_string(),
                        group(&[], &[], &[("infinibox", "estate")]),
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
        let all = entry.dataset.groups.get("all").expect("all was created");
        assert_eq!(all.vars.as_ref().unwrap()["infinibox"], "estate");
        assert_eq!(
            all.hosts,
            vec!["motoko.section9.net"],
            "without hosts the endpoint would prune it and the vars would vanish"
        );
    }

    // A group the target does not have yet is created, carrying the vars and no
    // hosts. Who is in it gets decided later and elsewhere -- by the next sync of
    // the source that owns membership, or by `group_by` at play time. Skipping it
    // lost every variable declared for a group whose members are not the
    // declaring source's to know.
    #[tokio::test]
    async fn a_group_the_target_does_not_have_is_created_vars_only() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: HashMap::new(),
                    groups: [(
                        "somewhere-else".to_string(),
                        group(&[], &[], &[("infinibox", "nope")]),
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
        let created = entry
            .dataset
            .groups
            .get("somewhere-else")
            .expect("the group is created rather than skipped");
        assert_eq!(created.vars.as_ref().unwrap()["infinibox"], "nope");
        assert!(
            created.hosts.is_empty(),
            "an enricher publishes what a group means, never who is in it"
        );
        // and still no host crossed over
        assert_eq!(entry.dataset.hostvars.len(), 1);
    }

    // A host's own entry in the source is genuinely per host, so it is still
    // copied onto the host.
    #[tokio::test]
    async fn a_hosts_own_source_vars_are_still_copied_onto_the_host() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
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
                    groups: HashMap::new(),
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

    fn enricher_without_fields() -> Enricher {
        serde_yaml_ng::from_str("name: d\ntarget_id: src-a\nsource_id: src-b\n")
            .expect("enricher fixture")
    }

    // No `fields` means every var the source declares -- the way being in a
    // group carries all of its vars in Ansible. It used to mean the opposite:
    // an enricher with none copied nothing and still reported success.
    #[tokio::test]
    async fn without_fields_every_var_is_taken() {
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
                        [("serial".to_string(), serde_json::json!("SN-1"))]
                            .into_iter()
                            .collect(),
                    )]
                    .into_iter()
                    .collect(),
                    groups: [(
                        "section9".to_string(),
                        group(&[], &[], &[("infinibox", "vol"), ("unrelated", "also")]),
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
            &enricher_without_fields(),
            None,
        )
        .await
        .expect("target is cached");

        let entry = cache.get("src-a").expect("entry");
        let vars = entry.dataset.groups["section9"]
            .vars
            .as_ref()
            .expect("vars");
        // both, where a fields list would have taken only what it named
        assert_eq!(vars["infinibox"], "vol");
        assert_eq!(vars["unrelated"], "also");
        // and the host's own vars from the source, all of them
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["serial"],
            "SN-1"
        );
        // still only the target's hosts: taking every var is not taking hosts
        assert_eq!(entry.dataset.hostvars.len(), 1);
    }

    // An empty list is not an absent one: it names nothing, so nothing travels.
    #[tokio::test]
    async fn an_empty_fields_list_still_takes_nothing() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(dataset(), 3600));
        cache.set(
            "src-b",
            CacheEntry::new(
                Dataset {
                    hostvars: [(
                        "motoko.section9.net".to_string(),
                        [("infinibox".to_string(), serde_json::json!("vol"))]
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

        let enricher: Enricher =
            serde_yaml_ng::from_str("name: d\ntarget_id: src-a\nsource_id: src-b\nfields: []\n")
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
        assert!(!entry.dataset.hostvars["motoko.section9.net"].contains_key("infinibox"));
    }

    fn enricher_yaml(extra: &str) -> Enricher {
        serde_yaml_ng::from_str(&format!(
            "name: d\ntarget_id: src-a\nsource_id: src-b\n{}",
            extra
        ))
        .expect("enricher fixture")
    }

    // Two sources of vars, one target, and the real shape of the problem: the
    // login every play needs sits on a tenancy group beside that tenancy's
    // password hashes, and another tenancy's group sits beside it. Only the
    // group name tells them apart -- no list of variable names can.
    fn cache_with_two_tenancies() -> MemoryCache {
        let cache = MemoryCache::new();
        cache.set(
            "src-a",
            CacheEntry::new(
                Dataset {
                    groups: [(
                        "tenancy_ours".to_string(),
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
                    groups: [
                        (
                            "all".to_string(),
                            group(
                                &[],
                                &[],
                                &[("cmdb_role", "device42"), ("bind_password", "s3cret")],
                            ),
                        ),
                        (
                            "tenancy_ours".to_string(),
                            group(
                                &[],
                                &[],
                                &[("useransible", "pq_ansible"), ("users_all", "hashes")],
                            ),
                        ),
                        (
                            "tenancy_theirs".to_string(),
                            group(
                                &[],
                                &[],
                                &[("useransible", "other"), ("users_all", "their-hashes")],
                            ),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    remove_hosts: Vec::new(),
                },
                3600,
            ),
        );
        cache
    }

    async fn run_with(cache: &MemoryCache, enricher: &Enricher) {
        run_enricher(
            cache,
            &SpyEnricher::default(),
            &SyncHealthRegistry::new(),
            std::path::Path::new("unused"),
            "en-d",
            enricher,
            None,
        )
        .await
        .expect("target is cached");
    }

    // groups_excluded keeps another tenancy's group out entirely, which is the
    // cut no `fields` list could make: the name that must travel and the name
    // that must not are both on a group, and both are called the same thing.
    #[tokio::test]
    async fn groups_excluded_keeps_another_tenancys_group_out() {
        let cache = cache_with_two_tenancies();
        run_with(
            &cache,
            &enricher_yaml("groups_excluded: [\"tenancy_theirs\"]\n"),
        )
        .await;

        let entry = cache.get("src-a").expect("entry");
        assert!(!entry.dataset.groups.contains_key("tenancy_theirs"));
        assert_eq!(
            entry.dataset.groups["tenancy_ours"].vars.as_ref().unwrap()["useransible"],
            "pq_ansible",
            "our own tenancy still arrives, hashes and all -- that is the repo's intent"
        );
    }

    // fields_excluded cuts on the other axis, for something sitting on a group
    // that must otherwise travel whole: `all`.
    #[tokio::test]
    async fn fields_excluded_drops_a_name_from_a_group_that_is_kept() {
        let cache = cache_with_two_tenancies();
        run_with(
            &cache,
            &enricher_yaml("fields_excluded: [\"bind_password\"]\n"),
        )
        .await;

        let all = cache.get("src-a").expect("entry").dataset.groups["all"]
            .vars
            .clone()
            .expect("all carries vars");
        assert_eq!(all["cmdb_role"], "device42");
        assert!(!all.contains_key("bind_password"));
    }

    // An explicit `groups` list is the whole list. `all` is exempt from needing
    // a match in the target, not from being selected against -- otherwise what
    // is written would not be what is taken.
    #[tokio::test]
    async fn an_explicit_groups_list_does_not_smuggle_all_in() {
        let cache = cache_with_two_tenancies();
        run_with(&cache, &enricher_yaml("groups: [\"tenancy_ours\"]\n")).await;

        let groups = &cache.get("src-a").expect("entry").dataset.groups;
        assert!(groups.contains_key("tenancy_ours"));
        assert!(
            !groups.contains_key("all"),
            "`all` was not named, so it is not taken"
        );
    }

    // Deny beats allow, on either axis, so the two can be combined without
    // wondering which wins.
    //
    // Excluding a group stops the source's vars reaching it. It does not delete
    // a group the TARGET owns -- `tenancy_ours` is src-a's own, with its own
    // host, and an enricher has no business removing it.
    #[tokio::test]
    async fn an_exclusion_beats_an_inclusion() {
        let cache = cache_with_two_tenancies();
        run_with(
            &cache,
            &enricher_yaml(
                "groups: [\"all\", \"tenancy_ours\"]\ngroups_excluded: [\"tenancy_ours\"]\n",
            ),
        )
        .await;

        let groups = &cache.get("src-a").expect("entry").dataset.groups;
        assert!(groups.contains_key("all"), "named and not excluded");
        assert!(
            groups["tenancy_ours"]
                .vars
                .as_ref()
                .is_none_or(|v| !v.contains_key("useransible")),
            "excluded: the target keeps its own group, the source's vars stay out"
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
