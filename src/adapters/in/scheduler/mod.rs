use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::AppState;
use crate::application::enrich::run_enricher;
use crate::application::projects::sync_project;
use crate::application::sync::{SyncScope, sync_source};
use crate::domain::project::GitProject;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::ports::cache::CachePort;
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;

// The scheduler is another "driving adapter", just like HTTP handlers:
// it triggers the same use cases from application/, just by time instead
// of by request. There is no business logic here — only timers and logs.

// Every loop below awaits its work inline, so a run that outlasts its interval
// leaves ticks behind it. tokio's default (`Burst`) then fires those ticks
// back-to-back to catch up: a sync that took an hour on a ten-minute interval
// is followed by five more with no pause between them — hammering, at exactly
// the moment the thing being synced is already struggling.
//
// `Skip` drops the ticks that were missed and resumes on the original schedule,
// so a slow run costs the runs it displaced and nothing more. `Delay` would
// also avoid the burst, but it shifts every later tick by the overrun, and a
// source configured to sync on the hour should stay on the hour.
fn ticker(interval_seconds: u64) -> Interval {
    let mut ticker = interval(Duration::from_secs(interval_seconds));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

// How long a source that takes its host list from another source waits for that
// source to have data before syncing anyway.
//
// Bounded rather than indefinite, because a dependency with no schedule of its
// own may never arrive and a task that waits forever is a task that never says
// why. When the budget runs out the sync goes ahead and fails exactly as it did
// before, which puts the reason in `sync_health` where an operator looks for it.
const DEPENDENCY_WAIT_SECONDS: u64 = 300;
const DEPENDENCY_POLL_SECONDS: u64 = 1;

// Returns true if the dependency turned up within the budget.
async fn wait_for_dependency(cache: &dyn CachePort, dependency: &str, budget: Duration) -> bool {
    tokio::time::timeout(budget, async {
        while cache.get(dependency).is_none() {
            tokio::time::sleep(Duration::from_secs(DEPENDENCY_POLL_SECONDS)).await;
        }
    })
    .await
    .is_ok()
}

pub fn start_sync_tasks(state: Arc<AppState>) {
    for (source_id, source) in &state.sources {
        let interval_secs = match source.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        let state = Arc::clone(&state);
        let source_id = source_id.clone();
        let source = source.clone();

        tokio::spawn(async move {
            info!(source = %source_id, interval_secs, "Source scheduled");

            // A source that resolves its host list from another source's cache
            // cannot sync until that source has data. Every source's first tick
            // fires at once, so at boot this one raced its dependency and lost:
            // it failed with "not in the cache yet — sync it first" and then
            // said nothing until its next interval, which on an hourly source
            // is an hour of a datacenter missing for no reason but start order.
            //
            // Waited for here, before the ticker exists, so it applies to the
            // first sync only. After boot an absent dependency is a real
            // failure and belongs in sync_health immediately, not behind a wait.
            if let Some(hosts_from) = &source.hosts_from_source {
                let budget = Duration::from_secs(DEPENDENCY_WAIT_SECONDS);
                if !wait_for_dependency(&*state.cache, &hosts_from.source, budget).await {
                    warn!(
                        source = %source_id,
                        dependency = %hosts_from.source,
                        waited_seconds = DEPENDENCY_WAIT_SECONDS,
                        "Source providing the host list has not synced — syncing anyway to record why"
                    );
                }
            }

            let mut ticker = ticker(interval_secs);

            loop {
                ticker.tick().await;
                info!(source = %source_id, "Syncing");

                let connector = state.connector_for(&source.connector_type);
                let enrichment = state.enrichment();
                let outcome = sync_source(
                    &*state.cache,
                    &**connector,
                    &*state.secrets,
                    &state.sync_health,
                    &state.syncs,
                    &source_id,
                    &source,
                    SyncScope::Full,
                    Some(&enrichment),
                )
                .await;

                match outcome.error {
                    None => {
                        info!(
                            source = %source_id,
                            hosts = outcome.total_hosts,
                            groups = outcome.total_groups,
                            "Synced"
                        );
                    }
                    Some(e) => {
                        error!(source = %source_id, error = %e, "Sync failed");
                    }
                }
            }
        });
    }

    start_enricher_tasks(state);
}

// Periodic re-pull of git project checkouts. Separate from start_sync_tasks
// because it doesn't need AppState: main wires it with its own git/secrets
// handles, before the HTTP router even exists.
pub fn start_project_sync_tasks(
    git: Arc<dyn GitPort>,
    secrets: Arc<dyn SecretsPort>,
    // The same registry instance AppState exposes: main hands it in because
    // these tasks start before the AppState exists
    health: Arc<SyncHealthRegistry>,
    projects: HashMap<String, GitProject>,
    projects_dir: PathBuf,
) {
    for (project_id, project) in projects {
        let interval_secs = match project.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        let git = Arc::clone(&git);
        let secrets = Arc::clone(&secrets);
        let health = Arc::clone(&health);
        let projects_dir = projects_dir.clone();

        tokio::spawn(async move {
            let mut ticker = ticker(interval_secs);
            // The boot sequence already cloned; skip the immediate first tick
            ticker.tick().await;

            info!(project = %project_id, interval_secs, "Project scheduled");

            loop {
                ticker.tick().await;
                match sync_project(
                    &*git,
                    &*secrets,
                    &health,
                    &project_id,
                    &project,
                    &projects_dir,
                )
                .await
                {
                    Ok(()) => info!(project = %project_id, "Project updated"),
                    Err(e) => error!(project = %project_id, error = %e, "Project update failed"),
                }
            }
        });
    }
}

fn start_enricher_tasks(state: Arc<AppState>) {
    for (enricher_id, enricher) in &state.enrichers {
        let interval_secs = match enricher.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        let state = Arc::clone(&state);
        let enricher_id = enricher_id.clone();
        let enricher = enricher.clone();

        tokio::spawn(async move {
            let mut ticker = ticker(interval_secs);

            info!(
                enricher = %enricher_id,
                target = %enricher.target_id,
                interval_secs,
                "Enricher scheduled"
            );

            loop {
                ticker.tick().await;
                info!(enricher = %enricher_id, "Running");

                match run_enricher(
                    &*state.cache,
                    &*state.enricher,
                    &state.enrich_health,
                    &enricher_id,
                    &enricher,
                )
                .await
                {
                    None => {
                        warn!(
                            enricher = %enricher_id,
                            target = %enricher.target_id,
                            "Target not in cache, skipping"
                        );
                    }
                    Some(outcome) => match outcome.error {
                        None => {
                            info!(
                                enricher = %enricher_id,
                                hosts_updated = outcome.hosts_updated,
                                hosts_removed = outcome.hosts_removed,
                                "Enriched"
                            );
                        }
                        Some(e) => {
                            error!(enricher = %enricher_id, error = %e, "Enrichment failed");
                        }
                    },
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::cache_entry::CacheEntry;
    use crate::domain::dataset::Dataset;

    fn empty_dataset() -> Dataset {
        Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_as_soon_as_the_dependency_has_data() {
        let cache = Arc::new(MemoryCache::new());

        let writer = Arc::clone(&cache);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            writer.set("src-inventory", CacheEntry::new(empty_dataset(), 60));
        });

        let start = tokio::time::Instant::now();
        let arrived = wait_for_dependency(&*cache, "src-inventory", Duration::from_secs(300)).await;

        assert!(arrived);
        // It returned when the data landed, not when the budget expired
        assert!(start.elapsed() < Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_dependency_already_in_cache_is_not_waited_for() {
        let cache = MemoryCache::new();
        cache.set("src-inventory", CacheEntry::new(empty_dataset(), 60));

        let start = tokio::time::Instant::now();
        assert!(wait_for_dependency(&cache, "src-inventory", Duration::from_secs(300)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    // Giving up is the point: the sync then runs and records why it failed,
    // instead of the task waiting forever and never reporting anything.
    #[tokio::test(start_paused = true)]
    async fn the_wait_gives_up_so_the_sync_can_report_the_failure() {
        let cache = MemoryCache::new();

        let start = tokio::time::Instant::now();
        let arrived = wait_for_dependency(&cache, "src-inventory", Duration::from_secs(300)).await;

        assert!(!arrived);
        assert!(start.elapsed() >= Duration::from_secs(300));
    }

    // `start_paused` runs the test on a virtual clock that jumps straight to
    // the next timer, so a 40-second schedule is exercised in microseconds
    // rather than making the suite wait for it.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_overruns_its_interval_does_not_burst() {
        let mut ticker = ticker(10);
        ticker.tick().await; // the first tick always fires immediately

        // The work took three and a half intervals
        tokio::time::sleep(Duration::from_secs(35)).await;

        // One tick is overdue and fires at once under any behaviour
        let start = tokio::time::Instant::now();
        ticker.tick().await;
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Here is the difference. Burst would fire the ticks missed while the
        // work ran, one immediately after another; Skip drops them and resumes
        // on the original schedule, which puts the next tick at t=40 — five
        // seconds away, not zero (Burst) and not ten (Delay, which would shift
        // every later tick by the overrun).
        let start = tokio::time::Instant::now();
        ticker.tick().await;
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(5),
            "missed ticks should be skipped and the original schedule resumed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_inside_its_interval_keeps_the_schedule() {
        let mut ticker = ticker(10);
        ticker.tick().await;

        tokio::time::sleep(Duration::from_secs(4)).await;

        let start = tokio::time::Instant::now();
        ticker.tick().await;
        assert_eq!(start.elapsed(), Duration::from_secs(6));
    }
}
