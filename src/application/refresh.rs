use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::application::sync::{
    DEFAULT_REFRESH_DEPTH, Enrichment, SyncRequest, SyncScope, sync_source,
};
use crate::domain::source::Source;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::ports::cache::CachePort;
use crate::ports::connector::ConnectorPort;
use crate::ports::secrets::SecretsPort;

// On-demand refresh: a READ that is allowed to go and get newer data first.
//
// The freshness policy is NOT a parameter of the request. The caller says only
// whether it would rather wait than be served stale data; how stale is too
// stale is the source's `ttl_seconds` (and its `ttl_overrides`), which the
// operator writes. Any consumer-supplied staleness bound would be a
// consumer-supplied load knob, and the load lands on somebody's datacenter.
//
// That single decision is what bounds the cost. A host is re-gathered at most
// once per TTL window however many consumers ask for it, so the worst case for
// a source is the load of `sync_interval_seconds == ttl_seconds` — a
// configuration the operator already knows how to reason about — and in
// practice much less, because only the hosts somebody actually looks at get
// refreshed at all.
//
// Two more limits sit under that one, for the cases the TTL window does not
// cover:
//
// - Concurrent requests for the SAME host inside one window would each start a
//   gather before the first finished, so they queue on a per-host lock and the
//   late ones re-check freshness instead of gathering again.
// - Requests for MANY DIFFERENT hosts at once are all "first in their window",
//   so the TTL does not bound them at all; a process-wide semaphore does.

pub struct RefreshCoordinator {
    // One lock per (source, host). Per host rather than per source on purpose:
    // a source-wide lock would make everyone wait behind a refresh of an
    // unreachable host, which is exactly the request that runs the full timeout.
    //
    // Entries are never removed. One is created per host that has ever been
    // refreshed, so the map is bounded by the inventory size (a key and an Arc,
    // tens of bytes) — cheaper than the bookkeeping to prune it safely.
    in_flight: DashMap<(String, String), Arc<Mutex<()>>>,
    limiter: Arc<Semaphore>,
    budget: Duration,
}

impl RefreshCoordinator {
    pub fn new(max_concurrent: usize, timeout_seconds: u64) -> Self {
        Self {
            in_flight: DashMap::new(),
            // A zero would deadlock every refresh forever, which is a
            // misconfiguration answering as a hang. One at a time is the
            // slowest thing that still works.
            limiter: Arc::new(Semaphore::new(max_concurrent.max(1))),
            budget: Duration::from_secs(timeout_seconds),
        }
    }

    fn lock_for(&self, source_id: &str, hostname: &str) -> Arc<Mutex<()>> {
        self.in_flight
            .entry((source_id.to_string(), hostname.to_string()))
            .or_default()
            .clone()
    }
}

// Why a read answered the way it did. The refresh is best effort by design:
// a read must not fail because the data behind it could not be improved.
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    // Hosts that were re-gathered for this request
    pub refreshed: Vec<String>,
    // Hosts already inside their TTL, so nothing was asked of anyone
    pub already_fresh: usize,
    // Set when a refresh was attempted and did not work. The response still
    // carries the cached data; this says not to trust it as current.
    pub error: Option<String>,
}

impl RefreshOutcome {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Default::default()
        }
    }
}

// Bring the named hosts of a source up to date, if they are not already, and
// only if the source allows it.
//
// `hosts` is always an explicit list. There is no "refresh everything" on this
// path: a whole-source refresh on a read is a gather of the entire datacenter
// triggered by opening a page, and the caller who genuinely wants that can
// POST a full sync. Naming the hosts is what keeps the cost of a read
// proportional to what the read returns.
// `ttl_override` replaces the source's DEFAULT TTL for this refresh — what a
// view passes when it declares a freshness policy of its own. It does not
// replace the source's per-host and per-group `ttl_overrides`, which keep
// winning: "an override beats the default" is the rule everywhere else, and a
// view must not be able to silently cancel the five-minute TTL somebody put on
// a critical host. None = the source's own TTL governs, as before.
#[allow(clippy::too_many_arguments)]
pub async fn refresh_hosts(
    cache: &dyn CachePort,
    connector: &dyn ConnectorPort,
    secrets: &dyn SecretsPort,
    health: &SyncHealthRegistry,
    coordinator: &RefreshCoordinator,
    source_id: &str,
    source: &Source,
    hosts: &[String],
    ttl_override: Option<u64>,
    // Forwarded to the sync: a refresh rewrites the hosts it gathers, so the
    // derived keys on those hosts need putting back too
    enrichment: Option<&Enrichment<'_>>,
) -> RefreshOutcome {
    if hosts.is_empty() {
        return RefreshOutcome::default();
    }

    let stale = stale_hosts(cache, source, source_id, hosts, ttl_override);
    if stale.is_empty() {
        record(source_id, "fresh");
        return RefreshOutcome {
            already_fresh: hosts.len(),
            ..Default::default()
        };
    }

    // Acquired in sorted order, which is what makes overlapping requests safe:
    // two callers asking for {a, b} and {b, c} would deadlock if each took its
    // own first choice. Sorting gives every caller the same order.
    let mut ordered: Vec<&String> = stale.iter().collect();
    ordered.sort();
    let mut _guards: Vec<OwnedMutexGuard<()>> = Vec::with_capacity(ordered.len());
    for hostname in &ordered {
        _guards.push(coordinator.lock_for(source_id, hostname).lock_owned().await);
    }

    // Re-checked under the locks: whoever held them may have just refreshed
    // these very hosts, in which case this request has nothing left to do. This
    // is the half of the coalescing that saves the work — the lock alone would
    // only serialise it.
    let still_stale = stale_hosts(cache, source, source_id, &stale, ttl_override);
    if still_stale.is_empty() {
        debug!(
            source = %source_id,
            hosts = stale.len(),
            "hosts were refreshed while waiting — nothing left to do"
        );
        record(source_id, "coalesced");
        return RefreshOutcome {
            already_fresh: hosts.len(),
            ..Default::default()
        };
    }

    let Ok(_permit) = coordinator.limiter.clone().acquire_owned().await else {
        // The semaphore is never closed; treat it as a failed refresh rather
        // than a panic if that ever changes.
        return RefreshOutcome::failed("refresh limiter unavailable".to_string());
    };

    // A budget of its own, independent of the source's `timeout_seconds`: that
    // one bounds a scheduled sync, which may reasonably take minutes. A consumer
    // waiting on a page cannot.
    let sync = sync_source(
        cache,
        connector,
        secrets,
        health,
        source_id,
        source,
        SyncRequest::refreshing_origin(
            SyncScope::Hosts(still_stale.clone()),
            DEFAULT_REFRESH_DEPTH,
        ),
        enrichment,
    );

    match timeout(coordinator.budget, sync).await {
        Ok(outcome) if outcome.success() => {
            debug!(
                source = %source_id,
                hosts = ?still_stale,
                duration_ms = outcome.duration_ms as u64,
                "refreshed on demand"
            );
            record(source_id, "refreshed");
            RefreshOutcome {
                already_fresh: hosts.len() - still_stale.len(),
                refreshed: still_stale,
                error: None,
            }
        }
        Ok(outcome) => {
            let error = outcome
                .error
                .unwrap_or_else(|| "sync reported no reason".to_string());
            warn!(
                source = %source_id,
                hosts = ?still_stale,
                error = %error,
                "on-demand refresh failed — serving the cached data"
            );
            record(source_id, "failed");
            RefreshOutcome::failed(error)
        }
        Err(_elapsed) => {
            let error = format!(
                "refresh did not finish within {}s",
                coordinator.budget.as_secs()
            );
            warn!(
                source = %source_id,
                hosts = ?still_stale,
                "on-demand refresh timed out — serving the cached data"
            );
            record(source_id, "timeout");
            RefreshOutcome::failed(error)
        }
    }
}

// Which of the named hosts are outside their TTL — including hosts that are not
// in the cache at all, which are as stale as data gets.
fn stale_hosts(
    cache: &dyn CachePort,
    source: &Source,
    source_id: &str,
    hosts: &[String],
    ttl_override: Option<u64>,
) -> Vec<String> {
    let Some(entry) = cache.get(source_id) else {
        // Nothing cached for this source yet: every named host needs fetching
        return hosts.to_vec();
    };

    let group_ttls = source.group_ttl_by_host(&entry.dataset);
    let default_ttl = ttl_override.unwrap_or(entry.ttl.as_secs());

    hosts
        .iter()
        .filter(|hostname| {
            let ttl = source.effective_ttl(hostname, &group_ttls, default_ttl);
            !entry.is_host_fresh(hostname, Some(ttl))
        })
        .cloned()
        .collect()
}

// Separated from the sync counters so "how much of my gathering load comes from
// consumers?" is answerable — the question the whole freshness-versus-load
// argument turns on.
fn record(source_id: &str, result: &str) {
    metrics::counter!(
        "unified_api_refresh_total",
        "source" => source_id.to_string(),
        "result" => result.to_string(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coordinator_hands_out_the_same_lock_for_the_same_host() {
        let coordinator = RefreshCoordinator::new(4, 10);
        let first = coordinator.lock_for("src-a", "h1.example");
        let again = coordinator.lock_for("src-a", "h1.example");
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn different_hosts_and_sources_get_different_locks() {
        let coordinator = RefreshCoordinator::new(4, 10);
        let a_h1 = coordinator.lock_for("src-a", "h1.example");
        assert!(!Arc::ptr_eq(
            &a_h1,
            &coordinator.lock_for("src-a", "h2.example")
        ));
        assert!(!Arc::ptr_eq(
            &a_h1,
            &coordinator.lock_for("src-b", "h1.example")
        ));
    }

    #[test]
    fn a_zero_concurrency_limit_is_clamped_rather_than_deadlocking() {
        let coordinator = RefreshCoordinator::new(0, 10);
        assert_eq!(coordinator.limiter.available_permits(), 1);
    }

    #[test]
    fn outcome_defaults_to_nothing_happened() {
        let outcome = RefreshOutcome::default();
        assert!(outcome.refreshed.is_empty());
        assert_eq!(outcome.already_fresh, 0);
        assert!(outcome.error.is_none());
    }
}
