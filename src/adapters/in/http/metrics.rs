use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use axum::extract::State;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::AppState;

// The metrics recorder is a process-wide global (like the tracing subscriber),
// so it can only be installed once. OnceLock makes repeated AppBuilder::build()
// calls — every integration test builds its own app — share the same recorder
// instead of failing on the second install.
static PROMETHEUS: OnceLock<PrometheusHandle> = OnceLock::new();

fn handle() -> &'static PrometheusHandle {
    PROMETHEUS.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus metrics recorder")
    })
}

// GET /metrics — Prometheus text exposition format. Public like the health
// probes: scrapers don't carry the API key.
pub async fn metrics(State(state): State<Arc<AppState>>) -> String {
    record_source_gauges(&state);
    handle().render()
}

// Freshness gauges, refreshed on every scrape.
//
// Why here and not on every sync: age grows with the clock, not with events.
// A gauge pushed at sync time would be frozen at "0 seconds old" until the
// next sync — reporting perfect freshness precisely when a source stops
// syncing, which is the failure this is meant to catch. Reading the cache at
// scrape time is one pass over the entries and is always current.
fn record_source_gauges(state: &AppState) {
    let cached: HashSet<String> = state.cache.keys().into_iter().collect();

    // Every CONFIGURED source reports whether it is in the cache at all, so a
    // source that has never synced is a 0 to alert on instead of an absent
    // series (which is indistinguishable from a renamed or removed source).
    for source_id in state.sources.keys() {
        metrics::gauge!("unified_api_source_cached", "source" => source_id.clone())
            .set(if cached.contains(source_id) { 1.0 } else { 0.0 });
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
    for (view_id, view) in &state.views {
        let snapshot = crate::application::views::snapshot(
            &*state.cache,
            &state.sources,
            view_id.as_str(),
            view,
        );

        metrics::gauge!("unified_api_view_fresh", "view" => view_id.clone())
            .set(if snapshot.is_fresh() { 1.0 } else { 0.0 });
        metrics::gauge!("unified_api_view_age_seconds", "view" => view_id.clone())
            .set(snapshot.age_seconds() as f64);
        metrics::gauge!("unified_api_view_ttl_seconds", "view" => view_id.clone())
            .set(snapshot.ttl_seconds() as f64);
        metrics::gauge!("unified_api_view_hosts", "view" => view_id.clone())
            .set(snapshot.hosts().len() as f64);

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

// Called from the composition root so the recorder exists before the first
// sync runs (metrics recorded before install are silently dropped).
pub fn init() {
    handle();
}
