use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::HeaderMap;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::AppState;
use crate::adapters::r#in::http::auth::{ApiKeys, match_token, presented_token};
use crate::adapters::r#in::http::error::ApiError;

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
        (status = 401, description = "server.metrics_require_auth is on and no valid API key \
         was presented", body = crate::adapters::r#in::http::error::ErrorBody)
    )
)]
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    axum::Extension(registry): axum::Extension<ApiKeys>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    if state.config().metrics_require_auth {
        let keys = registry.0.load();
        if !keys.is_empty() {
            let token = presented_token(&headers).ok_or_else(ApiError::missing_api_key)?;
            match_token(token, &keys).ok_or_else(ApiError::invalid_api_key)?;
        }
    }

    record_source_gauges(&state);
    record_task_health_gauges(&state);
    record_config_gauges(&state);
    Ok(handle().render())
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
    }

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
