use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

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

// The widest spread a task's schedule is shifted by, and the ceiling on how
// many intervals a failing task backs off to.
const MAX_JITTER_SECONDS: u64 = 30;
const MAX_BACKOFF_INTERVALS: u32 = 8;

// A per-task offset added before the first tick, so every task does not fire
// at the same instant — at boot that meant every source gathering at once
// (and, since tokio intervals keep their phase, colliding again at every
// common multiple forever).
//
// Deterministic (a hash of the id) rather than random: the same config spreads
// the same way on every boot, which makes load patterns reproducible, and it
// needs no RNG dependency. Capped at MAX_JITTER_SECONDS and at the interval
// itself — spreading a 10-second source across 30 seconds would be a schedule
// change, not a jitter.
fn startup_jitter(id: &str, interval_seconds: u64) -> Duration {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let window_ms = interval_seconds.min(MAX_JITTER_SECONDS) * 1000;
    if window_ms == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % window_ms)
}

// How many ticks a task sits out after a failure: 0 after the first (retry on
// the very next tick — most failures are transient), then exponentially more,
// capped at MAX_BACKOFF_INTERVALS. A failing source used to hammer its
// struggling target at exactly sync_interval_seconds forever; now the attempts
// land 1, 2, 4, 8, 8... intervals apart while `sync_health` carries the streak.
//
// Ticks rather than clock math: the ticker already owns the schedule (with
// Skip semantics), so backing off is just letting some ticks pass — attempts
// stay aligned to the configured cadence instead of drifting.
fn ticks_to_skip(consecutive_failures: u32) -> u32 {
    if consecutive_failures == 0 {
        return 0;
    }
    2u32.saturating_pow(consecutive_failures - 1)
        .min(MAX_BACKOFF_INTERVALS)
        - 1
}

// Spawn a periodic task through a supervisor: a panic in the body is counted,
// logged, and the body is restarted after `restart_delay` — instead of the
// tokio default, where the task dies silently and (for a sync task) that
// source simply stops syncing until someone notices the data went stale.
//
// `factory` builds a fresh body per (re)start, which is why it is a closure
// and not a future. A body that RETURNS is done on purpose (shutdown) and is
// not restarted; only a panic is.
fn spawn_supervised<F, Fut>(
    task: String,
    restart_delay: Duration,
    factory: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(factory()).await {
                Ok(()) => return,
                Err(e) if e.is_panic() => {
                    metrics::counter!(
                        "unified_api_scheduler_task_panics_total",
                        "task" => task.clone(),
                    )
                    .increment(1);
                    error!(task = %task, "Scheduler task panicked — restarting it");
                    tokio::time::sleep(restart_delay).await;
                }
                // Cancelled (runtime shutting down): nothing to restart into
                Err(_) => return,
            }
        }
    })
}

pub fn start_sync_tasks(state: Arc<AppState>) {
    for (source_id, source) in &state.sources {
        let interval_secs = match source.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        info!(source = %source_id, interval_secs, "Source scheduled");

        let task_state = Arc::clone(&state);
        let task_source_id = source_id.clone();
        let task_source = source.clone();

        spawn_supervised(
            format!("sync:{}", source_id),
            Duration::from_secs(interval_secs),
            move || {
                let state = Arc::clone(&task_state);
                let source_id = task_source_id.clone();
                let source = task_source.clone();

                async move {
                    tokio::time::sleep(startup_jitter(&source_id, interval_secs)).await;

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
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    loop {
                        ticker.tick().await;
                        if skip > 0 {
                            skip -= 1;
                            debug!(source = %source_id, ticks_left = skip, "Backing off, tick skipped");
                            continue;
                        }
                        info!(source = %source_id, "Syncing");

                        // Re-resolved every tick rather than once at task start: the
                        // checkout this script lives in may not have existed when the
                        // task spawned (boot no longer waits for clones), and a
                        // pipeline may move the script between runs.
                        let source = state
                            .source_for_sync(&source_id)
                            .unwrap_or_else(|| source.clone());

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
                                consecutive_failures = 0;
                                info!(
                                    source = %source_id,
                                    hosts = outcome.total_hosts,
                                    groups = outcome.total_groups,
                                    "Synced"
                                );
                            }
                            Some(e) => {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                skip = ticks_to_skip(consecutive_failures);
                                error!(
                                    source = %source_id,
                                    error = %e,
                                    next_attempt_in_intervals = skip + 1,
                                    "Sync failed"
                                );
                            }
                        }
                    }
                }
            },
        );
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

        info!(project = %project_id, interval_secs, "Project scheduled");

        let task_git = Arc::clone(&git);
        let task_secrets = Arc::clone(&secrets);
        let task_health = Arc::clone(&health);
        let task_projects_dir = projects_dir.clone();

        spawn_supervised(
            format!("project:{}", project_id),
            Duration::from_secs(interval_secs),
            move || {
                let git = Arc::clone(&task_git);
                let secrets = Arc::clone(&task_secrets);
                let health = Arc::clone(&task_health);
                let projects_dir = task_projects_dir.clone();
                let project_id = project_id.clone();
                let project = project.clone();

                async move {
                    tokio::time::sleep(startup_jitter(&project_id, interval_secs)).await;

                    let mut ticker = ticker(interval_secs);
                    // The boot sequence already cloned; skip the immediate first tick
                    ticker.tick().await;

                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    loop {
                        ticker.tick().await;
                        if skip > 0 {
                            skip -= 1;
                            debug!(project = %project_id, ticks_left = skip, "Backing off, tick skipped");
                            continue;
                        }
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
                            Ok(()) => {
                                consecutive_failures = 0;
                                info!(project = %project_id, "Project updated");
                            }
                            Err(e) => {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                skip = ticks_to_skip(consecutive_failures);
                                error!(
                                    project = %project_id,
                                    error = %e,
                                    next_attempt_in_intervals = skip + 1,
                                    "Project update failed"
                                );
                            }
                        }
                    }
                }
            },
        );
    }
}

fn start_enricher_tasks(state: Arc<AppState>) {
    for (enricher_id, enricher) in &state.enrichers {
        let interval_secs = match enricher.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        info!(
            enricher = %enricher_id,
            target = %enricher.target_id,
            interval_secs,
            "Enricher scheduled"
        );

        let task_state = Arc::clone(&state);
        let task_enricher_id = enricher_id.clone();
        let task_enricher = enricher.clone();

        spawn_supervised(
            format!("enrich:{}", enricher_id),
            Duration::from_secs(interval_secs),
            move || {
                let state = Arc::clone(&task_state);
                let enricher_id = task_enricher_id.clone();
                let enricher = task_enricher.clone();

                async move {
                    tokio::time::sleep(startup_jitter(&enricher_id, interval_secs)).await;

                    let mut ticker = ticker(interval_secs);
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    loop {
                        ticker.tick().await;
                        if skip > 0 {
                            skip -= 1;
                            debug!(enricher = %enricher_id, ticks_left = skip, "Backing off, tick skipped");
                            continue;
                        }
                        info!(enricher = %enricher_id, "Running");

                        // A missing target counts as a failure for backoff too:
                        // it is recorded as one in the health registry, and
                        // retrying a target nobody synced every interval is the
                        // same hammering as retrying a broken script.
                        let error = match run_enricher(
                            &*state.cache,
                            &*state.enricher,
                            &state.enrich_health,
                            &state.projects_dir,
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
                                true
                            }
                            Some(outcome) => match outcome.error {
                                None => {
                                    info!(
                                        enricher = %enricher_id,
                                        hosts_updated = outcome.hosts_updated,
                                        hosts_removed = outcome.hosts_removed,
                                        "Enriched"
                                    );
                                    false
                                }
                                Some(e) => {
                                    error!(enricher = %enricher_id, error = %e, "Enrichment failed");
                                    true
                                }
                            },
                        };

                        if error {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            skip = ticks_to_skip(consecutive_failures);
                        } else {
                            consecutive_failures = 0;
                        }
                    }
                }
            },
        );
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

    #[test]
    fn backoff_doubles_and_caps() {
        // Attempts land 1, 2, 4, 8 intervals apart, then stay at 8: the first
        // retry is immediate-ish (most failures are transient), the cap keeps
        // a dead source checking in often enough to notice a recovery.
        assert_eq!(ticks_to_skip(0), 0);
        assert_eq!(ticks_to_skip(1), 0);
        assert_eq!(ticks_to_skip(2), 1);
        assert_eq!(ticks_to_skip(3), 3);
        assert_eq!(ticks_to_skip(4), 7);
        assert_eq!(ticks_to_skip(5), 7);
        assert_eq!(ticks_to_skip(u32::MAX), 7);
    }

    #[test]
    fn jitter_is_bounded_and_deterministic() {
        // Within the window: never at or past min(interval, 30s)
        for id in ["src-a", "src-b", "src-c", "prj-d", "en-e"] {
            assert!(startup_jitter(id, 300) < Duration::from_secs(30), "{}", id);
            assert!(startup_jitter(id, 10) < Duration::from_secs(10), "{}", id);
        }
        // Deterministic: the same config spreads the same way on every boot
        assert_eq!(startup_jitter("src-a", 300), startup_jitter("src-a", 300));
        // And it actually spreads: these two ids land on different offsets
        // (deterministic hash, so this is a fixed fact, not a flaky one)
        assert_ne!(startup_jitter("src-a", 300), startup_jitter("src-b", 300));
    }

    #[test]
    fn zero_interval_means_zero_jitter() {
        assert_eq!(startup_jitter("src-a", 0), Duration::ZERO);
    }

    // The tokio default for a panicking task is silence: the JoinHandle is
    // dropped and that source simply stops syncing forever. The supervisor
    // restarts the body instead (and counts the panic).
    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_body_is_restarted() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runs = Arc::new(AtomicUsize::new(0));

        let factory_runs = Arc::clone(&runs);
        let handle = spawn_supervised(
            "test:panicky".to_string(),
            Duration::from_secs(5),
            move || {
                let runs = Arc::clone(&factory_runs);
                async move {
                    let run = runs.fetch_add(1, Ordering::SeqCst);
                    if run < 2 {
                        panic!("boom {}", run);
                    }
                    // Third run completes normally — the supervisor must NOT
                    // restart a body that returns
                }
            },
        );

        handle.await.expect("the supervisor itself must not panic");
        assert_eq!(runs.load(Ordering::SeqCst), 3);

        // Give the runtime a beat: if the supervisor wrongly restarted the
        // completed body, another run would have been counted
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }
}
