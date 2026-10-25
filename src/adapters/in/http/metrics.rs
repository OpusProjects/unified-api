use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::HeaderMap;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::AppState;
use crate::adapters::r#in::http::auth::{ApiKeys, match_token, presented_token};
use crate::adapters::r#in::http::error::ApiError;
use crate::domain::dataset::Dataset;

// The metrics recorder is a process-wide global (like the tracing subscriber),
// so it can only be installed once. OnceLock makes repeated AppBuilder::build()
// calls — every integration test builds its own app — share the same recorder
// instead of failing on the second install.
static PROMETHEUS: OnceLock<PrometheusHandle> = OnceLock::new();

// Bucket edges for every histogram in the process. Without them the exporter
// renders histograms as SUMMARIES (client-side quantiles), which cannot be
// aggregated across instances — avg(p99) of two pods is not a p99 of the
// fleet. Buckets can: sum the `_bucket` series and take histogram_quantile
// over the fleet. The spread covers both populations sharing the registry:
// HTTP requests (milliseconds) and script executions (seconds up to the
// 300-second default timeout).
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

fn handle() -> &'static PrometheusHandle {
    PROMETHEUS.get_or_init(|| {
        PrometheusBuilder::new()
            .set_buckets(DURATION_BUCKETS)
            .expect("bucket list is non-empty")
            .install_recorder()
            .expect("failed to install Prometheus metrics recorder")
    })
}

// Every HTTP request as a counter and a latency histogram, labeled by the
// MATCHED route pattern ("/api/v1/sources/{id}/dataset"), never the raw path:
// a per-host URL would mint a fresh label value per host and grow the
// registry without bound. Requests that matched no route share one
// "unmatched" label for the same reason. Status only on the counter — the
// route already dominates latency, and route × method × status on a
// 15-bucket histogram would multiply series for little insight.
pub async fn track_requests(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().as_str().to_string();
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let start = std::time::Instant::now();
    let response = next.run(request).await;

    metrics::counter!(
        "unified_api_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "unified_api_http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(start.elapsed().as_secs_f64());

    response
}

// GET /metrics — Prometheus text exposition format. Public by default like
// the health probes (scrapers don't carry the API key); with
// `server.metrics_require_auth: true` the handler requires one itself.
//
// Enforced HERE rather than by router placement, which is what lets the flag
// reload: the route is always registered public and consults the current
// snapshot on every scrape, so a flip takes effect on the next one. With no
// keys configured, authentication is off API-wide and the flag keeps having
// no effect — the same rule the middleware applies.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Health",
    responses(
        (status = 200, description = "Prometheus text exposition — counters, histograms and \
         scrape-time gauges (see docs/observability.md). Public by default; \
         `server.metrics_require_auth: true` requires the API key, since the \
         exposition labels every source id and host count", content_type = "text/plain"),
        (status = 401, description = "server.metrics_require_auth is on and neither a valid API \
         key nor a signed-in session was presented", body = crate::adapters::r#in::http::error::ErrorBody)
    )
)]
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    axum::Extension(registry): axum::Extension<ApiKeys>,
    sessions: Option<axum::Extension<crate::adapters::r#in::http::session::Sessions>>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    if state.config().metrics_require_auth {
        let keys = registry.0.load();
        if !keys.is_empty() {
            match presented_token(&headers) {
                Some(token) => {
                    match_token(token, &keys).ok_or_else(ApiError::invalid_api_key)?;
                }
                // No key in a header: a browser signed in to this instance
                // reads it with its session, like any other route. A read,
                // so no CSRF token is asked for.
                None => {
                    use crate::adapters::r#in::http::session::{COOKIE_NAME, cookie_value};
                    let signed_in = sessions.is_some_and(|axum::Extension(sessions)| {
                        headers
                            .get(axum::http::header::COOKIE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|cookies| cookie_value(cookies, COOKIE_NAME))
                            .and_then(|token| sessions.0.get(token))
                            .is_some()
                    });
                    if !signed_in {
                        return Err(ApiError::missing_api_key());
                    }
                }
            }
        }
    }

    record_source_gauges(&state);
    record_task_health_gauges(&state);
    record_config_gauges(&state);
    record_process_gauges();
    Ok(handle().render())
}

// Every hostname of every source, counted once, ignoring case: DNS does
// not distinguish `AASQL101` from `aasql101`, and sources disagree on it.
// Only a name that actually has capitals is copied to lowercase it; the rest
// stay borrowed.
//
// Case is the only folding done here. Merging `web01` into
// `web01.example.com`, or one domain into another, would need a rule that
// holds for every estate, and none does: `accesscontrol1` exists as three
// different machines in three datacenters.
fn unique_hosts(datasets: &[&Dataset]) -> usize {
    datasets
        .iter()
        .flat_map(|dataset| dataset.hostvars.keys())
        .map(|name| {
            if name.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(name.to_ascii_lowercase())
            } else {
                Cow::Borrowed(name.as_str())
            }
        })
        .collect::<HashSet<Cow<str>>>()
        .len()
}

// The process's own resource use, under the standard Prometheus names
// (process_resident_memory_bytes, process_cpu_seconds_total, ...) so stock
// dashboards and alerts work unchanged.
//
// Memory matters more here than in most services: the whole inventory cache
// lives in it, so resident memory is the number that says when a pod is
// about to meet its limit.
//
// Read from /proc at scrape time rather than through a collector crate: the
// files are a few hundred bytes, the reads are cheap enough for a blocking
// call on a scrape, and it adds nothing to the dependency tree. Linux only:
// that is where this runs (containers), and elsewhere the series are simply
// absent rather than wrong.
#[cfg(target_os = "linux")]
fn record_process_gauges() {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let stats = parse_proc_status(&status);
        if let Some(rss) = stats.resident_bytes {
            metrics::gauge!("process_resident_memory_bytes").set(rss as f64);
        }
        if let Some(virt) = stats.virtual_bytes {
            metrics::gauge!("process_virtual_memory_bytes").set(virt as f64);
        }
        if let Some(threads) = stats.threads {
            metrics::gauge!("process_threads").set(threads as f64);
        }
    }

    // A gauge under a counter's name: the exporter's counters are integers,
    // and CPU time in whole seconds would make rate() useless at the scale of
    // a quiet service. rate() works the same on either type.
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat")
        && let Some(seconds) = parse_stat_cpu_seconds(&stat)
    {
        metrics::gauge!("process_cpu_seconds_total").set(seconds);
    }

    // Less the one read_dir itself holds open while it lists the directory.
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        metrics::gauge!("process_open_fds").set(entries.count().saturating_sub(1) as f64);
    }
    if let Ok(limits) = std::fs::read_to_string("/proc/self/limits")
        && let Some(max) = parse_max_open_files(&limits)
    {
        metrics::gauge!("process_max_fds").set(max as f64);
    }
}

#[cfg(not(target_os = "linux"))]
fn record_process_gauges() {}

#[derive(Debug, Default, PartialEq)]
struct ProcStatus {
    resident_bytes: Option<u64>,
    virtual_bytes: Option<u64>,
    threads: Option<u64>,
}

// `/proc/self/status` reports memory in kB ("VmRSS:   123456 kB"), which
// sidesteps the page size that /proc/self/statm would need.
fn parse_proc_status(text: &str) -> ProcStatus {
    let mut stats = ProcStatus::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let number = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok());
        match key {
            "VmRSS" => stats.resident_bytes = number.map(|kb| kb * 1024),
            "VmSize" => stats.virtual_bytes = number.map(|kb| kb * 1024),
            "Threads" => stats.threads = number,
            _ => {}
        }
    }
    stats
}

// User plus system CPU time of the whole process, every thread, from
// `/proc/self/stat` (fields 14 and 15). Not /proc/self/schedstat: that file
// is per TASK, so it counts only the main thread, while the work runs on the
// runtime's worker threads, and the rate read close to zero under load.
//
// The values are in clock ticks of USER_HZ, which Linux fixes at 100 for
// everything it reports under /proc regardless of the kernel's own tick rate.
// The process name (field 2) is in parentheses and may itself contain spaces
// or parentheses, so the fields are counted from after the LAST ')'.
const USER_HZ: f64 = 100.0;

fn parse_stat_cpu_seconds(text: &str) -> Option<f64> {
    let after_name = &text[text.rfind(')')? + 1..];
    // Field 3 (state) is the first after the name: utime is field 14 and
    // stime field 15, so 11 and 12 here.
    let mut fields = after_name.split_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((utime + stime) as f64 / USER_HZ)
}

// The soft limit from the "Max open files" row of /proc/self/limits.
// "unlimited" has no number to report, so the series is left out.
fn parse_max_open_files(text: &str) -> Option<u64> {
    let row = text
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    row.trim_start_matches("Max open files")
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

// Which build and which configuration this process is running — the first
// questions a fleet dashboard asks now that a pod's configuration can change
// without a restart. Scrape-time like everything here.
fn record_config_gauges(state: &AppState) {
    // The classic build-info idiom: a constant 1 whose label carries the
    // value, so any other series can be joined onto the version.
    metrics::gauge!("unified_api_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    // 0 at boot, bumped by every applied reload — one query shows the pod
    // whose generation lags a fleet-wide configuration push.
    metrics::gauge!("unified_api_config_generation").set(state.reload.generation() as f64);

    // How many restart-only keys the last applied reload changed. Non-zero =
    // this pod runs on a configuration it could only partially adopt, and
    // only a restart clears it. A count, not per-key labels: the count is
    // what an alert needs, and GET /api/v1/config already answers "which".
    metrics::gauge!("unified_api_config_restart_required")
        .set(state.restart_pending().len() as f64);
}

// Freshness gauges, refreshed on every scrape.
//
// Why here and not on every sync: age grows with the clock, not with events.
// A gauge pushed at sync time would be frozen at "0 seconds old" until the
// next sync — reporting perfect freshness precisely when a source stops
// syncing, which is the failure this is meant to catch. Reading the cache at
// scrape time is one pass over the entries and is always current.
fn record_source_gauges(state: &AppState) {
    let config = state.config();
    let cached: HashSet<String> = state.cache.keys().into_iter().collect();

    // Every CONFIGURED source reports whether it is in the cache at all, so a
    // source that has never synced is a 0 to alert on instead of an absent
    // series (which is indistinguishable from a renamed or removed source).
    for source_id in config.sources.keys() {
        metrics::gauge!("unified_api_source_cached", "source" => source_id.clone())
            .set(if cached.contains(source_id) { 1.0 } else { 0.0 });

        // Sync health, exported instead of trapped in /status. The freshness
        // gauges above say how old the data is; these say whether anything is
        // still managing to refresh it — the difference between "syncs hourly,
        // fine" and "failing since Tuesday", which until now could only be
        // alerted on through the lossy `unified_api_source_fresh == 0` proxy.
        // (`last_error` stays API-only: an error string as a label value is
        // unbounded cardinality.)
        //
        // Scrape-time like everything here, and the attempt age doubly earns
        // it: a scheduler task that stops running pushes nothing, so only a
        // clock-driven gauge can show its silence — the attempt age just keeps
        // growing.
        if let Some(health) = state.sync_health.get(source_id) {
            metrics::gauge!("unified_api_source_sync_consecutive_failures", "source" => source_id.clone())
                .set(health.consecutive_failures as f64);
            if let Some(age) = health.last_attempt_age_seconds {
                metrics::gauge!("unified_api_source_sync_last_attempt_age_seconds", "source" => source_id.clone())
                    .set(age as f64);
            }
            // Absent until the first success: a source that has NEVER synced
            // successfully has no age to report, and an absent series says so
            if let Some(age) = health.last_success_age_seconds {
                metrics::gauge!("unified_api_source_sync_last_success_age_seconds", "source" => source_id.clone())
                    .set(age as f64);
            }
        }
    }

    // Driven by the cache, not by config: an entry can outlive its config
    // entry (source removed from YAML, cache still serving it) and that is
    // exactly the state an operator wants to see.
    // Kept until the loop ends, so the unique count below can borrow every
    // hostname instead of copying ~tens of thousands of strings per scrape.
    let mut entries = Vec::with_capacity(cached.len());
    for source_id in cached {
        let Some(entry) = state.cache.get(&source_id) else {
            // Evicted between keys() and get() — it will show up next scrape
            continue;
        };

        metrics::gauge!("unified_api_source_age_seconds", "source" => source_id.clone())
            .set(entry.age_seconds() as f64);
        metrics::gauge!("unified_api_source_ttl_seconds", "source" => source_id.clone())
            .set(entry.ttl.as_secs() as f64);
        metrics::gauge!("unified_api_source_fresh", "source" => source_id.clone())
            .set(if entry.is_fresh() { 1.0 } else { 0.0 });
        metrics::gauge!("unified_api_source_hosts", "source" => source_id.clone())
            .set(entry.dataset.hostvars.len() as f64);
        metrics::gauge!("unified_api_source_groups", "source" => source_id.clone())
            .set(entry.dataset.groups.len() as f64);
        entries.push(entry);
    }

    // The number of unique machines the inventory knows. Summing
    // unified_api_source_hosts counts a host once per source it appears in
    // (Device42, vCenter, monitoring and SSH facts all list the same box), so
    // the sum overstates the estate several times over. This is the union of
    // hostnames across every cached source: exact names, so a host listed as
    // `web01` in one source and `web01.example.com` in another still counts
    // twice, which is a naming difference worth seeing rather than hiding.
    let datasets: Vec<&Dataset> = entries.iter().map(|entry| entry.dataset.as_ref()).collect();
    metrics::gauge!("unified_api_hosts_unique").set(unique_hosts(&datasets) as f64);

    record_view_gauges(state);
}

// The same questions for views, which the source gauges cannot answer.
//
// A view holds no cache entry — it is resolved from its members on every read —
// so it appears in neither `cache.keys()` nor `state.sources`, and had no series
// at all. That left the one address consumers are pointed at as the one thing
// impossible to alert on: every member could be healthy while the view served
// nothing, because ownership resolves against an inventory source that has not
// synced.
//
// Separate metric names rather than reusing `unified_api_source_*` with a view
// id. The two id spaces are shared on the ROUTES deliberately, but a view's
// hosts are its members' hosts — folding them into one series would double-count
// every host in any sum across the label.
fn record_view_gauges(state: &AppState) {
    let config = state.config();
    for (view_id, view) in &config.views {
        let snapshot = crate::application::views::snapshot(
            &*state.cache,
            &config.sources,
            &state.advertised_scopes,
            view_id.as_str(),
            view,
        );

        metrics::gauge!("unified_api_view_fresh", "view" => view_id.clone())
            .set(if snapshot.is_fresh() { 1.0 } else { 0.0 });
        metrics::gauge!("unified_api_view_age_seconds", "view" => view_id.clone())
            .set(snapshot.age_seconds() as f64);
        metrics::gauge!("unified_api_view_ttl_seconds", "view" => view_id.clone())
            .set(snapshot.ttl_seconds() as f64);
        // The host-union count is the one O(hosts) number here; memoized on
        // the cache generation so an idle instance answers scrapes without
        // rebuilding the union every 15 seconds (see ViewHostsMemo).
        let hosts = state
            .view_hosts_memo
            .hosts_count(view_id, state.cache.generation(), || snapshot.hosts().len());
        metrics::gauge!("unified_api_view_hosts", "view" => view_id.clone()).set(hosts as f64);

        // How much of the view is actually assembled. `members_cached` short of
        // `members_total` is a view serving part of its inventory; a member
        // whose OWNERSHIP source has not synced claims nothing beyond literally
        // named hosts, which is the state where a view 404s hosts that plainly
        // exist — and it is invisible in every other number here.
        let cached = snapshot
            .members
            .iter()
            .filter(|member| member.entry.is_some())
            .count();
        let routable = snapshot
            .members
            .iter()
            .filter(|member| member.ownership_cached())
            .count();

        metrics::gauge!("unified_api_view_members_total", "view" => view_id.clone())
            .set(snapshot.members.len() as f64);
        metrics::gauge!("unified_api_view_members_cached", "view" => view_id.clone())
            .set(cached as f64);
        metrics::gauge!("unified_api_view_members_routable", "view" => view_id.clone())
            .set(routable as f64);
    }
}

// Health of the periodic work that is NOT a source sync: enricher runs,
// project pulls, and the cache snapshot task. Their registries record last
// attempt / last success / consecutive failures; exporting them is what turns
// "a warn! per interval" into something an alert can fire on.
//
// Computed at scrape time like the freshness gauges, and for the same reason:
// the ages grow with the clock, so a value pushed when the task last ran would
// freeze exactly when the task stops running. Keyed by the CONFIGURED ids, so
// a configured-but-never-run job simply has no series yet (like a source with
// no cache entry), and a job removed from config stops being re-set.
fn record_task_health_gauges(state: &AppState) {
    let config = state.config();
    for enricher_id in config.enrichers.keys() {
        if let Some(health) = state.enrich_health.get(enricher_id) {
            metrics::gauge!("unified_api_enricher_consecutive_failures", "enricher" => enricher_id.clone())
                .set(health.consecutive_failures as f64);
            if let Some(age) = health.last_success_age_seconds {
                metrics::gauge!("unified_api_enricher_last_success_age_seconds", "enricher" => enricher_id.clone())
                    .set(age as f64);
            }
        }
    }

    for project_id in config.projects.keys() {
        if let Some(health) = state.project_health.get(project_id) {
            metrics::gauge!("unified_api_project_sync_consecutive_failures", "project" => project_id.clone())
                .set(health.consecutive_failures as f64);
            if let Some(age) = health.last_success_age_seconds {
                metrics::gauge!("unified_api_project_sync_last_success_age_seconds", "project" => project_id.clone())
                    .set(age as f64);
            }
        }
    }

    // One snapshot task per process: no label. Note that an idle cache skips
    // its snapshots on purpose, so last_success age grows while nothing
    // changes — alert on consecutive_failures, which only moves on real
    // attempts.
    if let Some(health) = state
        .snapshot_health
        .get(crate::adapters::out::cache::persistence::SNAPSHOT_HEALTH_KEY)
    {
        metrics::gauge!("unified_api_snapshot_consecutive_failures")
            .set(health.consecutive_failures as f64);
        if let Some(age) = health.last_success_age_seconds {
            metrics::gauge!("unified_api_snapshot_last_success_age_seconds").set(age as f64);
        }
    }
}

// Called from the composition root so the recorder exists before the first
// sync runs (metrics recorded before install are silently dropped).
pub fn init() {
    handle();
}

#[cfg(test)]
mod process_tests {
    use super::*;

    // The estate, not the sum: a host listed by three sources is one host,
    // whatever case each source writes it in.
    #[test]
    fn a_host_in_several_sources_counts_once() {
        let parse = |json: &str| serde_json::from_str::<Dataset>(json).unwrap();
        let d42 = parse(r#"{"hostvars": {"web01": {}, "db01": {}}}"#);
        let vmware = parse(r#"{"hostvars": {"web01": {}, "app01": {}}}"#);
        let facts = parse(r#"{"hostvars": {"WEB01": {}, "web01.example.com": {}}}"#);
        // web01 (in any case), db01, app01, and the FQDN spelling as its own
        // name: only case is folded, never domains
        assert_eq!(unique_hosts(&[&d42, &vmware, &facts]), 4);
        assert_eq!(unique_hosts(&[]), 0);
    }

    #[test]
    fn status_memory_is_reported_in_bytes() {
        let status = "Name:\tunified-api\nVmSize:\t  2048 kB\nVmRSS:\t   512 kB\nThreads:\t17\n";
        assert_eq!(
            parse_proc_status(status),
            ProcStatus {
                resident_bytes: Some(512 * 1024),
                virtual_bytes: Some(2048 * 1024),
                threads: Some(17),
            }
        );
    }

    #[test]
    fn stat_cpu_time_is_user_plus_system_in_seconds() {
        // A name with spaces and a ')' of its own must not shift the fields.
        let line = "4242 (tokio rt) worker)) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    250 150 0 0 20 0 9 0 12345 0 0";
        assert_eq!(parse_stat_cpu_seconds(line), Some(4.0));
        assert_eq!(parse_stat_cpu_seconds(""), None);
        assert_eq!(parse_stat_cpu_seconds("1 (x) S"), None);
    }

    #[test]
    fn max_open_files_takes_the_soft_limit() {
        let limits = "Limit                     Soft Limit           Hard Limit           Units\n\
                      Max open files            1024                 524288               files\n";
        assert_eq!(parse_max_open_files(limits), Some(1024));
        let unlimited =
            "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_max_open_files(unlimited), None);
    }

    // On the platform this ships on, the real files parse into real numbers.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_reports_its_own_memory() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        assert!(parse_proc_status(&status).resident_bytes.unwrap() > 0);
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        assert!(parse_stat_cpu_seconds(&stat).is_some());
    }
}
