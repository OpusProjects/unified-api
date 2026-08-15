use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::application::credentials::resolve_credentials;
use crate::application::enrich::run_enrichers_for_target;
use crate::domain::cache_entry::{CacheEntry, RetainedHost};
use crate::domain::dataset::HostVars;
use crate::domain::enricher::Enricher;
use crate::domain::source::Source;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::sync_mode::SyncMode;
use crate::ports::cache::CachePort;
use crate::ports::connector::{ConnectorOutput, ConnectorPort};
use crate::ports::enricher::EnricherPort;
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

// How many federation hops a refresh request may travel before it stops being
// propagated. Three covers edge → region → global with a hop to spare; the
// point is that a topology accidentally wired into a cycle stops instead of
// generating an infinite storm of gathers.
pub const DEFAULT_REFRESH_DEPTH: u8 = 3;

// What to sync: the slice (scope) and whether to make the origin re-gather it
// first (refresh_origin).
//
// The two are deliberately separate. The scope is a transfer optimisation — it
// says which hosts are wanted. refresh_origin is an intent that travels down a
// federation chain: a central holding an edge's data as a source cannot produce
// newer data by itself, it can only ask the edge to go and get it. Without the
// flag a sync means "give me what you have", which is what a scheduled sync
// must keep meaning: a central pulling on its own interval must never turn into
// SSH load on the edge.
pub struct SyncRequest {
    pub scope: SyncScope,
    pub refresh_origin: bool,
    // Remaining hops. Each federated connector propagates depth - 1 and stops
    // propagating at zero.
    pub refresh_depth: u8,
    // WHO asked: the HTTP request id for a manual sync, "scheduled" for a
    // timer-driven one, "refresh" for an on-demand read. Travels to the
    // connector script inside SOURCE_CONFIG as the reserved `trigger` key, so
    // a script's own logs can join the same trace as the access log. None =
    // unstated (tests, internal callers) — the key is simply absent.
    pub trigger: Option<String>,
}

impl SyncRequest {
    // The plain sync: whatever the source can give without asking anyone else
    pub fn new(scope: SyncScope) -> Self {
        Self {
            scope,
            refresh_origin: false,
            refresh_depth: 0,
            trigger: None,
        }
    }

    pub fn refreshing_origin(scope: SyncScope, refresh_depth: u8) -> Self {
        Self {
            scope,
            refresh_origin: true,
            refresh_depth,
            trigger: None,
        }
    }

    pub fn with_trigger(mut self, trigger: impl Into<String>) -> Self {
        self.trigger = Some(trigger.into());
        self
    }
}

// So every existing caller can keep passing a bare scope: the scheduler and
// every plain `POST /sync` mean "no refresh", which is the From below.
impl From<SyncScope> for SyncRequest {
    fn from(scope: SyncScope) -> Self {
        Self::new(scope)
    }
}

// One lock per source, so two syncs of the same source run one after the other.
//
// The scheduler and `POST /sync` call the same use case with nothing between
// them, so a manual sync can land in the middle of a scheduled one. The cost is
// not only the duplicated gather — it is that the two finish in an order nobody
// chose. A slow full sync writes the dataset it collected minutes earlier over
// a scoped refresh that just fetched the truth, and `CacheEntry::new` stamps
// every host "now", so the stale value is labelled freshly gathered and the next
// refresh declines to correct it. The consumer asked for current facts, got
// older ones, and the freshness data agrees with the wrong answer.
//
// Per SOURCE rather than per host, unlike the refresh coordinator: the unit of a
// sync is the whole entry, and a full sync rewrites all of it.
//
// Entries are never removed — one per configured source, a key and an Arc — the
// same trade the refresh coordinator makes for the same reason.
//
// Serialising alone is not enough: N concurrent full syncs become N SEQUENTIAL
// full gathers — a form spamming its refresh button costs the datacenter N
// complete inventories. `full_syncs` counts the successful plain full syncs of
// each source; a caller that queued for the lock and finds the counter moved
// past what it saw before queueing knows the whole source was gathered while
// it waited — a sync that STARTED after its request began, which is everything
// it could have asked for. It answers from that instead of gathering again
// (the same reasoning as the read path re-checking staleness under its lock).
#[derive(Default)]
struct SourceSync {
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    full_syncs: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
pub struct SyncCoordinator {
    in_flight: dashmap::DashMap<String, SourceSync>,
}

impl SyncCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry_for(
        &self,
        source_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Mutex<()>>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        let entry = self.in_flight.entry(source_id.to_string()).or_default();
        (entry.lock.clone(), entry.full_syncs.clone())
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
    // True when no gather ran here: a full sync that started after this
    // request began completed while it queued, and this outcome reports what
    // that sync left in the cache
    pub coalesced: bool,
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
            coalesced: false,
        }
    }
}

// The use case "sync a source": resolve credentials, execute
// the connector, and apply the result to cache based on scope and sync_mode.
//
// The caller chooses the connector (ProcessConnector or SshConnector, based on
// source.connector_type) and passes it already resolved — this way this function only
// depends on ports, not on AppState.
// What a sync needs to put enrichment back after it writes.
//
// Bundled so sync_source grows by one parameter instead of two, and optional
// because a deployment with no enrichers configured should not have to build
// one. Borrowed rather than owned: the caller already holds both in AppState.
pub struct Enrichment<'a> {
    pub port: &'a dyn EnricherPort,
    pub enrichers: &'a HashMap<String, Enricher>,
    // Enricher runs record their outcome (keyed by enricher id), wherever
    // they are triggered from — the enrichers' own health registry, not the
    // sources' one
    pub health: &'a SyncHealthRegistry,
    // Where project checkouts live, so an enricher's script resolves into its
    // checkout at execution time (see application::scripts)
    pub projects_dir: &'a std::path::Path,
}

#[allow(clippy::too_many_arguments)]
pub async fn sync_source(
    cache: &dyn CachePort,
    connector: &dyn ConnectorPort,
    secrets: &dyn SecretsPort,
    health: &SyncHealthRegistry,
    // Serialises syncs of the same source. Held for the whole use case — gather,
    // cache write and enrichment — because all three are the sync's effect on
    // one entry, and a second sync interleaving with any of them is what lets an
    // older gather win.
    syncs: &SyncCoordinator,
    source_id: &str,
    source: &Source,
    // `impl Into<SyncRequest>` accepts both a bare SyncScope (the plain sync
    // every existing caller does) and a full SyncRequest, so adding the refresh
    // intent did not have to touch the scheduler or any test
    request: impl Into<SyncRequest>,
    // None = do not re-apply enrichment (a caller with none configured, or a
    // test that only cares about the gather)
    enrichment: Option<&Enrichment<'_>>,
) -> SyncOutcome {
    let request = request.into();
    // Only a PLAIN full sync can stand in for another: a scoped sync gathers a
    // slice, and refresh_origin demands the origin re-gather — a completed
    // ordinary sync satisfies neither.
    let coalescable = matches!(request.scope, SyncScope::Full) && !request.refresh_origin;

    let (lock, full_syncs) = syncs.entry_for(source_id);
    let full_syncs_before = full_syncs.load(std::sync::atomic::Ordering::SeqCst);
    let _guard = lock.lock_owned().await;

    if coalescable && full_syncs.load(std::sync::atomic::Ordering::SeqCst) != full_syncs_before {
        // The whole source was gathered by a sync that started after this
        // request began: answer from it instead of paying for an identical
        // gather. Nothing is recorded in sync health (no attempt ran here —
        // the winner recorded its own) and enrichment is not re-applied (the
        // winner did that too). Only the counter says it happened.
        let (total_hosts, total_groups) = cache
            .get(source_id)
            .map(|entry| (entry.dataset.hostvars.len(), entry.dataset.groups.len()))
            .unwrap_or((0, 0));
        metrics::counter!(
            "unified_api_sync_total",
            "source" => source_id.to_string(),
            "result" => "coalesced",
        )
        .increment(1);
        return SyncOutcome {
            scope: SyncScope::Full.label(),
            total_hosts,
            total_groups,
            duration_ms: 0,
            error: None,
            coalesced: true,
        };
    }

    let outcome = run_sync(cache, connector, secrets, source_id, source, request).await;

    // Recorded here rather than at the call sites so the scheduler and the HTTP
    // handler cannot drift: every sync in the process goes through this
    // function. Until now a failed scheduled sync left nothing behind but a log
    // line, so a stale dataset could not be told apart from a slow one.
    // A sync replaces what it wrote, so whatever an enricher had added to
    // those hosts went with it. Re-apply here, at the one place every sync in
    // the process passes through, so no caller can forget — the same reason
    // sync health is recorded here and not at the call sites. The enricher's
    // own interval stays as the backstop for the write paths that do not come
    // through this function.
    if let Some(enrichment) = enrichment.filter(|_| outcome.success()) {
        let applied = run_enrichers_for_target(
            cache,
            enrichment.port,
            enrichment.health,
            enrichment.projects_dir,
            enrichment.enrichers,
            source_id,
        )
        .await;
        if applied > 0 {
            tracing::debug!(
                source = source_id,
                enrichers = applied,
                "Re-applied enrichment after sync"
            );
        }
    }

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

    // Bumped only under the lock and only for the plain full case, so a
    // waiter comparing before/after cannot be satisfied by a scoped slice or
    // a failed attempt — a failed winner means the next in line gathers for
    // real, which is the retry a caller would want.
    if coalescable && outcome.success() {
        full_syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    outcome
}

async fn run_sync(
    cache: &dyn CachePort,
    connector: &dyn ConnectorPort,
    secrets: &dyn SecretsPort,
    source_id: &str,
    source: &Source,
    request: SyncRequest,
) -> SyncOutcome {
    let SyncRequest {
        scope,
        refresh_origin,
        refresh_depth,
        trigger,
    } = request;
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

    // Who asked, for the script's own logs — same reserved-key channel as
    // scope/target, visible inside SOURCE_CONFIG
    if let Some(trigger) = trigger {
        config.insert("trigger".to_string(), trigger);
    }

    // Only federated connectors act on this: they are the ones with an origin
    // to ask. A local connector (script, ssh, static inventory) IS the origin —
    // its sync already gathers fresh data — so it ignores both keys.
    if refresh_origin {
        config.insert("refresh_origin".to_string(), "true".to_string());
        config.insert("refresh_depth".to_string(), refresh_depth.to_string());
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
                coalesced: false,
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
    let ConnectorOutput {
        dataset,
        ages,
        unreachable,
    } = output;
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
                // The closure runs only when the entry already existed, so it
                // doubles as "was there anything here before?"
                let mut occupied = false;
                cache.merge_or_insert(
                    source_id,
                    dataset,
                    source.ttl_seconds,
                    &mut |entry, _new| {
                        occupied = true;
                        for (hostname, vars, age) in &updates {
                            entry.update_host_aged(hostname.clone(), vars.clone(), *age);
                        }
                    },
                );

                // Nothing was here: merge_or_insert took its insert branch and
                // built a CacheEntry::new, which gets two things wrong for a
                // scoped sync. It stamps every host "now" — on a cold central
                // (no scheduled sync yet, a consumer asking for a host) that
                // throws away the origin's ages this whole path exists to
                // preserve. And it presents the dataset as freshly gathered,
                // though this sync gathered hosts and not the source. Both are
                // corrected here, in one atomic pass.
                if !occupied {
                    cache.update(source_id, &mut |entry| {
                        for (hostname, _vars, age) in &updates {
                            if let Some(vars) = entry.dataset.hostvars.get(hostname).cloned() {
                                entry.update_host_aged(hostname.clone(), vars, *age);
                            }
                        }
                        entry.mark_partial();
                    });
                }
            }
        }
        SyncScope::Group(group) => {
            let mut occupied = false;
            cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                occupied = true;
                entry.update_group(group, new)
            });

            // Same as the host scope: a group is not the source, so an entry
            // this sync had to create has no full gather behind it.
            if !occupied {
                cache.update(source_id, &mut |entry| entry.mark_partial());
            }
        }
        SyncScope::Full => match source.sync_mode {
            SyncMode::Replace if unreachable.is_empty() => {
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
            // Same replace, except the hosts the connector could not reach keep
            // the data they already had. A gather that fails is our problem, not
            // evidence the host is gone: one saturated batch of SSH workers used
            // to take every host in it out of the inventory until the next run,
            // so a healthy server would come and go on a two-hour cycle.
            //
            // Only hosts still offered upstream can be here — one no longer
            // listed is never attempted, so it is absent from `unreachable` and
            // is dropped exactly as before. That is what keeps this from
            // accumulating ghosts.
            //
            // They keep their PREVIOUS age rather than being stamped now: a
            // retained host is as stale as it truly is, so the TTL still expires
            // it and a refresh still targets it. Stamping it fresh would turn an
            // intermittent gap into stale data presented as current.
            //
            // What is kept is the host's WHOLE previous state, group memberships
            // included. A connector derives its groups from the hosts it managed
            // to gather, so keeping only the variables left the host in the
            // dataset and in no group — still gone for every consumer that
            // renders an inventory from `groups`, which is most of them.
            SyncMode::Replace => {
                let ttl = source.ttl_seconds;
                cache.merge_or_insert(source_id, dataset, ttl, &mut |entry, new| {
                    let retained: Vec<RetainedHost> = unreachable
                        .iter()
                        .filter(|host| !new.hostvars.contains_key(*host))
                        .filter_map(|host| entry.capture_host(host))
                        .collect();

                    // The closure replaces the entry wholesale, which is what
                    // Replace means; merge_or_insert only lends us the previous
                    // one first so the retained hosts can be read out of it
                    // under the same lock.
                    *entry = match &ages {
                        Some(a) => CacheEntry::restore(
                            new,
                            ttl,
                            a.dataset_age_seconds,
                            a.host_ages.clone(),
                        ),
                        None => CacheEntry::new(new, ttl),
                    };

                    for host in retained {
                        entry.restore_host(host);
                    }
                });
            }
            SyncMode::Merge => {
                cache.merge_or_insert(source_id, dataset, source.ttl_seconds, &mut |entry, new| {
                    entry.merge_dataset(new);
                    // A full sync landed. It patched rather than replaced, so
                    // nothing else here would say so — but the source as a whole
                    // has now been gathered: the dataset clock restarts, and an
                    // entry that scoped syncs had marked partial stops being one.
                    //
                    // Merge preserves hosts upstream no longer lists, which is
                    // about what the entry CONTAINS. It says nothing about when
                    // the gather happened, and the gather happened now.
                    entry.mark_gathered();
                });
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::dataset::Dataset;

    fn replace_source() -> Source {
        serde_yaml_ng::from_str(
            "name: test\nproject_id: test\nscript_path: x\nschedule: null\nttl_seconds: 3600\nsync_mode: replace\n",
        )
        .expect("source fixture")
    }

    fn host(name: &str, role: &str) -> (String, HostVars) {
        (
            name.to_string(),
            [("role".to_string(), serde_json::json!(role))]
                .into_iter()
                .collect(),
        )
    }

    fn dataset_of(hosts: Vec<(String, HostVars)>) -> Dataset {
        Dataset {
            hostvars: hosts.into_iter().collect(),
            groups: std::collections::HashMap::new(),
            remove_hosts: Vec::new(),
        }
    }

    // The shape an SSH source produces: groups derived from the hosts that
    // actually answered, so a host that did not answer is in none of them.
    fn dataset_of_grouped(
        hosts: Vec<(String, HostVars)>,
        groups: Vec<(&str, Vec<&str>)>,
    ) -> Dataset {
        use crate::domain::dataset::Group;

        Dataset {
            hostvars: hosts.into_iter().collect(),
            groups: groups
                .into_iter()
                .map(|(name, members)| {
                    (
                        name.to_string(),
                        Group {
                            hosts: members.into_iter().map(String::from).collect(),
                            children: Vec::new(),
                            vars: None,
                        },
                    )
                })
                .collect(),
            remove_hosts: Vec::new(),
        }
    }

    // A gather that fails is our problem, not evidence the host is gone.
    #[test]
    fn replace_keeps_a_host_that_did_not_answer() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(
                dataset_of(vec![host("a.example", "web"), host("b.example", "db")]),
                3600,
            ),
        );

        // b did not answer this round, so the connector names it
        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "web")]),
                ages: None,
                unreachable: vec!["b.example".to_string()],
            },
        );

        let entry = cache.get("src").expect("entry");
        assert_eq!(entry.dataset.hostvars["b.example"]["role"], "db");
        assert_eq!(entry.dataset.hostvars["a.example"]["role"], "web");
    }

    // The other half of the distinction: upstream no longer lists the host, so
    // it was never attempted, is absent from `unreachable`, and still goes.
    #[test]
    fn replace_drops_a_host_upstream_stopped_listing() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(
                dataset_of(vec![host("a.example", "web"), host("b.example", "db")]),
                3600,
            ),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "web")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        let entry = cache.get("src").expect("entry");
        assert!(!entry.dataset.hostvars.contains_key("b.example"));
    }

    // A retained host must stay as stale as it truly is: stamping it fresh
    // would stop the TTL expiring it and stop a refresh ever retrying it.
    #[test]
    fn a_retained_host_keeps_its_age() {
        let cache = MemoryCache::new();
        let source = replace_source();
        let mut previous = CacheEntry::new(dataset_of(vec![host("b.example", "db")]), 3600);
        let (name, vars) = host("b.example", "db");
        previous.update_host_aged(name, vars, 900);
        cache.set("src", previous);

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "web")]),
                ages: None,
                unreachable: vec!["b.example".to_string()],
            },
        );

        let entry = cache.get("src").expect("entry");
        assert!(
            entry.host_age_seconds("b.example").unwrap_or(0) >= 900,
            "a retained host must not be stamped fresh"
        );
        assert!(!entry.is_host_fresh("b.example", Some(600)));
    }

    // Keeping the variables is only half of keeping the host: a consumer that
    // renders an inventory from `groups` — which is what an Ansible inventory
    // is — still could not see it.
    #[test]
    fn a_retained_host_keeps_its_group_membership() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(
                dataset_of_grouped(
                    vec![host("a.example", "web"), host("b.example", "db")],
                    vec![
                        ("ssh_gathered", vec!["a.example", "b.example"]),
                        ("ansible_os_family", vec!["a.example", "b.example"]),
                    ],
                ),
                3600,
            ),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of_grouped(
                    vec![host("a.example", "web")],
                    vec![
                        ("ssh_gathered", vec!["a.example"]),
                        ("ansible_os_family", vec!["a.example"]),
                    ],
                ),
                ages: None,
                unreachable: vec!["b.example".to_string()],
            },
        );

        let entry = cache.get("src").expect("entry");
        for group in ["ssh_gathered", "ansible_os_family"] {
            assert!(
                entry.dataset.groups[group]
                    .hosts
                    .contains(&"b.example".to_string()),
                "retained host missing from group '{}'",
                group
            );
        }
    }

    // A group disappears from a gather exactly when every host in it failed to
    // answer, so it has to come back with them.
    #[test]
    fn a_group_whose_hosts_all_failed_comes_back_with_them() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(
                dataset_of_grouped(
                    vec![host("a.example", "web"), host("b.example", "db")],
                    vec![
                        ("ssh_gathered", vec!["a.example", "b.example"]),
                        ("oracle_version", vec!["b.example"]),
                    ],
                ),
                3600,
            ),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of_grouped(
                    vec![host("a.example", "web")],
                    vec![("ssh_gathered", vec!["a.example"])],
                ),
                ages: None,
                unreachable: vec!["b.example".to_string()],
            },
        );

        let entry = cache.get("src").expect("entry");
        assert_eq!(entry.dataset.groups["oracle_version"].hosts, ["b.example"]);
    }

    // The other half of the distinction, at group level: a host upstream
    // stopped listing is not retained, so its groups do not come back either.
    #[test]
    fn a_dropped_host_does_not_resurrect_its_groups() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(
                dataset_of_grouped(
                    vec![host("a.example", "web"), host("b.example", "db")],
                    vec![("oracle_version", vec!["b.example"])],
                ),
                3600,
            ),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "web")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        let entry = cache.get("src").expect("entry");
        assert!(!entry.dataset.groups.contains_key("oracle_version"));
    }

    // The TTL lives in config, and a merge-mode source's entry is patched in
    // place rather than replaced — so before this it kept whatever TTL it was
    // created with. Lowering `ttl_seconds` had no effect, and with disk
    // persistence the old value came back on every restart.
    #[test]
    fn a_merge_sync_adopts_a_changed_ttl() {
        let cache = MemoryCache::new();
        let source: Source = serde_yaml_ng::from_str(
            "name: test\nproject_id: test\nscript_path: x\nschedule: null\nttl_seconds: 60\nsync_mode: merge\n",
        )
        .expect("source fixture");

        // The entry was created back when the source was configured for a day
        cache.set(
            "src",
            CacheEntry::new(dataset_of(vec![host("a.example", "web")]), 86400),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("b.example", "db")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        assert_eq!(cache.get("src").expect("entry").ttl.as_secs(), 60);
    }

    // A merge-mode source patches its entry in place, so nothing renewed the
    // dataset-level clock: the age grew from the first sync forever and the
    // source read as stale one TTL after boot, however faithfully it had been
    // syncing since. An operator alerting on `unified_api_source_fresh` got a
    // permanent alarm for a perfectly healthy source.
    //
    // Backdated by seconds rather than hours because Instant is monotonic from
    // boot: a CI runner has not been up for two hours.
    #[test]
    fn a_merge_sync_renews_the_dataset_clock() {
        let cache = MemoryCache::new();
        let source: Source = serde_yaml_ng::from_str(
            "name: test\nproject_id: test\nscript_path: x\nschedule: null\nttl_seconds: 1\nsync_mode: merge\n",
        )
        .expect("source fixture");

        cache.set(
            "src",
            CacheEntry::restore(
                dataset_of(vec![host("a.example", "web")]),
                1,
                5,
                std::collections::HashMap::new(),
            ),
        );
        assert!(!cache.get("src").expect("entry").is_fresh());

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Full,
            ConnectorOutput {
                dataset: dataset_of(vec![host("b.example", "db")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        let entry = cache.get("src").expect("entry");
        assert_eq!(entry.age_seconds(), 0);
        assert!(
            entry.is_fresh(),
            "a merge sync must renew the dataset clock"
        );
        // and it merged rather than replaced: both hosts are here
        assert!(entry.dataset.hostvars.contains_key("a.example"));
        assert!(entry.dataset.hostvars.contains_key("b.example"));
    }

    // A consumer asking for one host on a cold cache creates the entry. It used
    // to report age 0 and fresh for the whole source — one host presented as a
    // freshly gathered datacenter, and a source that has never fully synced
    // hidden from the gauge meant to catch exactly that.
    #[test]
    fn a_host_scoped_sync_on_a_cold_cache_does_not_claim_a_full_dataset() {
        let cache = MemoryCache::new();
        let source = replace_source();

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Hosts(vec!["a.example".to_string()]),
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "web")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        let entry = cache.get("src").expect("entry");
        assert!(!entry.is_complete());
        assert!(!entry.is_fresh(), "one host is not a gathered source");
        // The host itself was genuinely gathered, and says so
        assert!(entry.is_host_fresh("a.example", None));
    }

    #[test]
    fn a_group_scoped_sync_on_a_cold_cache_does_not_claim_a_full_dataset() {
        let cache = MemoryCache::new();
        let source = replace_source();

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Group("web".to_string()),
            ConnectorOutput {
                dataset: dataset_of_grouped(
                    vec![host("a.example", "web")],
                    vec![("web", vec!["a.example"])],
                ),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        assert!(!cache.get("src").expect("entry").is_fresh());
    }

    #[test]
    fn a_host_scoped_sync_leaves_an_existing_full_entry_complete() {
        let cache = MemoryCache::new();
        let source = replace_source();
        cache.set(
            "src",
            CacheEntry::new(dataset_of(vec![host("a.example", "web")]), 3600),
        );

        apply_to_cache(
            &cache,
            "src",
            &source,
            &SyncScope::Hosts(vec!["a.example".to_string()]),
            ConnectorOutput {
                dataset: dataset_of(vec![host("a.example", "db")]),
                ages: None,
                unreachable: Vec::new(),
            },
        );

        assert!(cache.get("src").expect("entry").is_fresh());
    }

    // The way out of the partial state: a sync of the whole source.
    #[test]
    fn a_later_full_sync_makes_the_entry_complete() {
        for mode in ["replace", "merge"] {
            let cache = MemoryCache::new();
            let source: Source = serde_yaml_ng::from_str(&format!(
                "name: test\nproject_id: test\nscript_path: x\nschedule: null\nttl_seconds: 3600\nsync_mode: {}\n",
                mode
            ))
            .expect("source fixture");

            apply_to_cache(
                &cache,
                "src",
                &source,
                &SyncScope::Hosts(vec!["a.example".to_string()]),
                ConnectorOutput {
                    dataset: dataset_of(vec![host("a.example", "web")]),
                    ages: None,
                    unreachable: Vec::new(),
                },
            );
            assert!(!cache.get("src").expect("entry").is_fresh(), "{}", mode);

            apply_to_cache(
                &cache,
                "src",
                &source,
                &SyncScope::Full,
                ConnectorOutput {
                    dataset: dataset_of(vec![host("a.example", "web"), host("b.example", "db")]),
                    ages: None,
                    unreachable: Vec::new(),
                },
            );

            let entry = cache.get("src").expect("entry");
            assert!(entry.is_complete(), "{} mode left the entry partial", mode);
            assert!(entry.is_fresh(), "{}", mode);
        }
    }

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

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::adapters::out::secrets::mock::MockSecrets;
    use crate::domain::dataset::Dataset;
    use crate::domain::source::OutputFormat;
    use crate::ports::connector::ConnectorResult;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    // A connector where a FULL gather is slow and a host-scoped one is instant,
    // and each stamps the host with which gather produced it. That is the shape
    // that exposes the ordering: the slow one starts first and finishes last.
    struct PacedConnector {
        full_delay: Duration,
    }

    impl ConnectorPort for PacedConnector {
        fn execute(
            &self,
            _script_path: &str,
            _args: &[String],
            _output_format: OutputFormat,
            config: &HashMap<String, String>,
            _credentials: &HashMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
            let scoped = config.get("scope").map(String::as_str) == Some("host");
            let delay = if scoped {
                Duration::ZERO
            } else {
                self.full_delay
            };
            let marker = if scoped { "scoped" } else { "full" };

            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let vars: HostVars = [("gathered_by".to_string(), serde_json::json!(marker))]
                    .into_iter()
                    .collect();
                Ok(Dataset {
                    hostvars: [("a.example".to_string(), vars)].into_iter().collect(),
                    groups: HashMap::new(),
                    remove_hosts: Vec::new(),
                }
                .into())
            })
        }
    }

    fn source() -> Source {
        serde_yaml_ng::from_str(
            "name: test\nproject_id: test\nscript_path: x\nschedule: null\nttl_seconds: 3600\nsync_mode: replace\n",
        )
        .expect("source fixture")
    }

    // A slow full sync must not overwrite a scoped gather that started later and
    // finished first. Without serialisation it does: the full sync writes the
    // data it collected before the refresh even began, and CacheEntry::new
    // stamps every host "now" — so the consumer's refresh is silently undone
    // AND the stale value is labelled freshly gathered, which stops the next
    // refresh from correcting it.
    #[tokio::test]
    async fn a_slow_full_sync_does_not_clobber_a_later_scoped_gather() {
        let cache = Arc::new(MemoryCache::new());
        let connector = Arc::new(PacedConnector {
            full_delay: Duration::from_millis(300),
        });
        let secrets = Arc::new(MockSecrets::new());
        let health = Arc::new(SyncHealthRegistry::new());
        // ONE coordinator, shared by both syncs — the same instance AppState holds
        let syncs = Arc::new(SyncCoordinator::new());
        let src = source();

        let full = {
            let (cache, connector, secrets, health, syncs, src) = (
                Arc::clone(&cache),
                Arc::clone(&connector),
                Arc::clone(&secrets),
                Arc::clone(&health),
                Arc::clone(&syncs),
                src.clone(),
            );
            tokio::spawn(async move {
                sync_source(
                    &*cache,
                    &*connector,
                    &*secrets,
                    &health,
                    &syncs,
                    "src",
                    &src,
                    SyncScope::Full,
                    None,
                )
                .await
            })
        };

        // let the full gather get under way, then refresh one host
        tokio::time::sleep(Duration::from_millis(50)).await;
        sync_source(
            &*cache,
            &*connector,
            &*secrets,
            &health,
            &syncs,
            "src",
            &src,
            SyncScope::Hosts(vec!["a.example".to_string()]),
            None,
        )
        .await;

        full.await.expect("full sync task");

        let entry = cache.get("src").expect("entry");
        assert_eq!(
            entry.dataset.hostvars["a.example"]["gathered_by"], "scoped",
            "the slow full sync overwrote a gather that started after it"
        );
    }

    // A connector that counts its executions and takes long enough for other
    // requests to queue behind the first.
    struct CountingConnector {
        delay: Duration,
        executions: std::sync::atomic::AtomicUsize,
        fail_first: bool,
    }

    impl ConnectorPort for CountingConnector {
        fn execute(
            &self,
            _script_path: &str,
            _args: &[String],
            _output_format: OutputFormat,
            _config: &HashMap<String, String>,
            _credentials: &HashMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
            let run = self
                .executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let delay = self.delay;
            let fail = self.fail_first && run == 0;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                if fail {
                    return Err(crate::ports::connector::ConnectorError {
                        message: "first gather blew up".to_string(),
                        stderr: String::new(),
                        exit_code: Some(1),
                    });
                }
                let vars: HostVars = [("role".to_string(), serde_json::json!("web"))]
                    .into_iter()
                    .collect();
                Ok(Dataset {
                    hostvars: [("a.example".to_string(), vars)].into_iter().collect(),
                    groups: HashMap::new(),
                    remove_hosts: Vec::new(),
                }
                .into())
            })
        }
    }

    struct SyncHarness {
        cache: Arc<MemoryCache>,
        connector: Arc<CountingConnector>,
        secrets: Arc<MockSecrets>,
        health: Arc<SyncHealthRegistry>,
        syncs: Arc<SyncCoordinator>,
        source: Source,
    }

    impl SyncHarness {
        fn new(connector: CountingConnector) -> Self {
            Self {
                cache: Arc::new(MemoryCache::new()),
                connector: Arc::new(connector),
                secrets: Arc::new(MockSecrets::new()),
                health: Arc::new(SyncHealthRegistry::new()),
                syncs: Arc::new(SyncCoordinator::new()),
                source: source(),
            }
        }

        fn spawn(&self, scope: SyncScope) -> tokio::task::JoinHandle<SyncOutcome> {
            let (cache, connector, secrets, health, syncs, src) = (
                Arc::clone(&self.cache),
                Arc::clone(&self.connector),
                Arc::clone(&self.secrets),
                Arc::clone(&self.health),
                Arc::clone(&self.syncs),
                self.source.clone(),
            );
            tokio::spawn(async move {
                sync_source(
                    &*cache,
                    &*connector,
                    &*secrets,
                    &health,
                    &syncs,
                    "src",
                    &src,
                    scope,
                    None,
                )
                .await
            })
        }
    }

    // The 0.11.0 mutex serialized concurrent syncs but did not deduplicate
    // them: N requests were N sequential full datacenter gathers. A full sync
    // that started after a request began counts as that request's.
    #[tokio::test]
    async fn concurrent_full_syncs_cost_one_gather() {
        let harness = SyncHarness::new(CountingConnector {
            delay: Duration::from_millis(300),
            executions: std::sync::atomic::AtomicUsize::new(0),
            fail_first: false,
        });

        let first = harness.spawn(SyncScope::Full);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = harness.spawn(SyncScope::Full);
        let third = harness.spawn(SyncScope::Full);

        let outcomes = [
            first.await.expect("task"),
            second.await.expect("task"),
            third.await.expect("task"),
        ];

        assert!(outcomes.iter().all(|o| o.success()));
        assert!(
            outcomes.iter().all(|o| o.total_hosts == 1),
            "coalesced outcomes report what the winner left in the cache"
        );
        assert_eq!(
            outcomes.iter().filter(|o| o.coalesced).count(),
            2,
            "the winner gathered, the two queued requests coalesced onto it"
        );
        assert_eq!(
            harness
                .connector
                .executions
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "three requests must cost the origin one gather"
        );
    }

    // Only full-on-full coalesces: a scoped sync names the hosts it wants
    // freshly gathered, and answering it from a bulk sync would hand back
    // whatever ages that gather produced instead of the targeted re-gather
    // the caller explicitly paid for.
    #[tokio::test]
    async fn a_scoped_sync_queued_behind_a_full_one_still_gathers() {
        let harness = SyncHarness::new(CountingConnector {
            delay: Duration::from_millis(200),
            executions: std::sync::atomic::AtomicUsize::new(0),
            fail_first: false,
        });

        let full = harness.spawn(SyncScope::Full);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let scoped = harness.spawn(SyncScope::Hosts(vec!["a.example".to_string()]));

        assert!(full.await.expect("task").success());
        let scoped = scoped.await.expect("task");
        assert!(scoped.success());
        assert!(!scoped.coalesced);
        assert_eq!(
            harness
                .connector
                .executions
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    // A failed winner satisfies nobody: the next in line gathers for real,
    // which is exactly the retry the caller would have wanted.
    #[tokio::test]
    async fn a_failed_sync_does_not_satisfy_the_queue() {
        let harness = SyncHarness::new(CountingConnector {
            delay: Duration::from_millis(200),
            executions: std::sync::atomic::AtomicUsize::new(0),
            fail_first: true,
        });

        let first = harness.spawn(SyncScope::Full);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = harness.spawn(SyncScope::Full);

        assert!(!first.await.expect("task").success());
        let second = second.await.expect("task");
        assert!(second.success(), "the retry gathered for real");
        assert!(!second.coalesced);
        assert_eq!(
            harness
                .connector
                .executions
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }
}
