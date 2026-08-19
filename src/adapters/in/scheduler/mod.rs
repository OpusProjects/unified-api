use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::AppState;
use crate::application::enrich::run_enricher;
use crate::application::projects::sync_project;
use crate::application::sync::{SyncRequest, SyncScope, sync_source};
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

// Parse a source's `schedule` cron expression. Standard 5-field cron with an
// OPTIONAL leading seconds field (6 fields), evaluated in UTC — containers
// run UTC and a schedule that shifts with the host's timezone database is a
// surprise nobody asked for. Config validation calls this at startup, so a
// bad expression fails the deploy naming the source, never the first tick.
pub(crate) fn parse_cron(expression: &str) -> Result<croner::Cron, String> {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .build()
        .parse(expression)
        .map_err(|e| e.to_string())
}

// How long until the schedule's next occurrence after `now`. None when the
// pattern has no future occurrence at all (croner gives up past year 5000 —
// practically a misconfiguration).
fn next_cron_delay(cron: &croner::Cron, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let next = cron.find_next_occurrence(&now, false).ok()?;
    (next - now).to_std().ok()
}

// How a source's sync task paces itself: a fixed interval (jittered, ticks
// with Skip semantics) or a cron schedule (exact times, deliberately
// unjittered — "on the hour" was chosen by a person, and shifting it would be
// a schedule change). Backoff works identically in both: a failing source
// lets occurrences pass, 1, 2, 4, up to 8 apart.
// The Cron variant is boxed for size (a parsed pattern is ~300 bytes, an
// interval is 8): one Cadence exists per scheduled source, so this is about
// clippy's variant-size lint, not memory that matters.
#[derive(Clone)]
enum Cadence {
    Interval(u64),
    Cron(Box<croner::Cron>),
}

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

// One scheduled sync attempt with the backoff bookkeeping, shared by the
// interval and cron loops so the two cannot drift.
async fn scheduled_sync(
    state: &AppState,
    source_id: &str,
    fallback: &crate::domain::source::Source,
    consecutive_failures: &mut u32,
    skip: &mut u32,
) {
    info!(source = %source_id, "Syncing");

    // Re-resolved every attempt rather than once at task start: the checkout
    // this script lives in may not have existed when the task spawned (boot
    // does not wait for clones), and a pipeline may move the script between
    // runs.
    let source = state
        .source_for_sync(source_id)
        .unwrap_or_else(|| fallback.clone());

    let connector = state.connector_for(&source.connector_type);
    let config = state.config();
    let enrichment = state.enrichment(&config);
    let outcome = sync_source(
        &*state.cache,
        &**connector,
        &*state.secrets,
        &state.sync_health,
        &state.advertised_scopes,
        &state.syncs,
        source_id,
        &source,
        SyncRequest::new(SyncScope::Full).with_trigger("scheduled"),
        Some(&enrichment),
    )
    .await;

    match outcome.error {
        None => {
            *consecutive_failures = 0;
            info!(
                source = %source_id,
                hosts = outcome.total_hosts,
                groups = outcome.total_groups,
                // true = this attempt piggybacked on a manual sync that
                // finished while it queued
                coalesced = outcome.coalesced,
                "Synced"
            );
        }
        Some(e) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            *skip = ticks_to_skip(*consecutive_failures);
            error!(
                source = %source_id,
                error = %e,
                next_attempt_in_occurrences = *skip + 1,
                "Sync failed"
            );
        }
    }
}

// One scheduled enricher run with the backoff bookkeeping, shared by the
// interval and cron loops so the two cannot drift.
async fn scheduled_enrichment(
    state: &AppState,
    enricher_id: &str,
    enricher: &crate::domain::enricher::Enricher,
    consecutive_failures: &mut u32,
    skip: &mut u32,
) {
    info!(enricher = %enricher_id, "Running");

    // A missing target counts as a failure for backoff too: it is recorded as
    // one in the health registry, and retrying a target nobody synced every
    // interval is the same hammering as retrying a broken script.
    let error = match run_enricher(
        &*state.cache,
        &*state.enricher,
        &state.enrich_health,
        &state.projects_dir,
        enricher_id,
        enricher,
        Some("scheduled"),
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
        *consecutive_failures = consecutive_failures.saturating_add(1);
        *skip = ticks_to_skip(*consecutive_failures);
    } else {
        *consecutive_failures = 0;
    }
}

// One scheduled project pull with the backoff bookkeeping, shared by the
// interval and cron loops so the two cannot drift.
#[allow(clippy::too_many_arguments)]
async fn scheduled_project_pull(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn crate::ports::venv::VenvPort,
    health: &SyncHealthRegistry,
    project_id: &str,
    project: &GitProject,
    projects_dir: &std::path::Path,
    consecutive_failures: &mut u32,
    skip: &mut u32,
) {
    match sync_project(
        git,
        secrets,
        venv,
        health,
        project_id,
        project,
        projects_dir,
    )
    .await
    {
        Ok(()) => {
            *consecutive_failures = 0;
            info!(project = %project_id, "Project updated");
        }
        Err(e) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            *skip = ticks_to_skip(*consecutive_failures);
            error!(
                project = %project_id,
                error = %e,
                next_attempt_in_occurrences = *skip + 1,
                "Project update failed"
            );
        }
    }
}

// Every start_* function takes the shutdown receiver and returns the spawned
// handles: on SIGTERM main flips the watch, each body returns at its next
// wait point (a running sync is finished, never cut mid-write), and main joins
// the handles before the final snapshot — so the snapshot cannot serialize a
// cache a sync task is still mutating.
// The scheduler as something that can be REBUILT, not only started.
//
// Every periodic task captures the source (or enricher, or project) it was
// spawned for, which is exactly what makes it cheap — and exactly what makes
// it wrong the moment the configuration changes underneath it. So a reload
// does not try to talk to the running tasks: it replaces them. One generation
// of tasks is told to stop, a new generation is spawned from the new
// snapshot, and the difference is invisible to everything else.
//
// The outgoing generation is NOT waited for here. Its tasks stop at their
// next wait point, which for a task in the middle of a gather means after
// that gather finishes — the same "never cut mid-write" rule shutdown
// follows. Waiting would put a datacenter-wide sync between a pipeline's push
// and its response. It is safe to overlap because SyncCoordinator already
// serialises syncs of one source: the outgoing task's last gather and the
// incoming task's first cannot interleave, whichever order they arrive in.
//
// Shutdown still drains everything, outgoing generations included, because
// main's final cache snapshot may not run while any task can still write.
pub fn start_supervisor(
    state: Arc<AppState>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reload = state.reload.subscribe();
        // Generations that have been told to stop and are finishing their
        // in-flight work. Pruned on every reload so a long-lived process does
        // not accumulate handles of tasks that ended hours ago.
        let mut draining: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        loop {
            if *shutdown.borrow() {
                return;
            }

            let generation = state.reload.generation();
            let (stop, stop_rx) = watch::channel(false);

            let mut handles = start_sync_tasks(Arc::clone(&state), stop_rx.clone());
            handles.extend(start_project_tasks(Arc::clone(&state), stop_rx.clone()));
            // A project that arrived WITH this reload has no checkout yet, and
            // the scripts of any source pointing into it would be missing
            // until its first periodic pull — which for a project without an
            // interval is never. Boot already clones (main), so this is only
            // for what a reload added.
            if generation > 0 {
                handles.extend(clone_missing_checkouts(Arc::clone(&state)));
            }

            info!(generation, tasks = handles.len(), "Scheduler tasks started");

            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = stop.send(true);
                    draining.extend(handles);
                    for handle in draining {
                        let _ = handle.await;
                    }
                    return;
                }
                _ = reload.changed() => {
                    let _ = stop.send(true);
                    draining.retain(|handle| !handle.is_finished());
                    draining.extend(handles);
                }
            }
        }
    })
}

// The project tasks, wired from the state rather than from main's locals —
// which is what lets a reload restart them with a different projects.yaml.
fn start_project_tasks(
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let projects = state.config().projects.clone();
    if projects.is_empty() {
        return Vec::new();
    }
    start_project_sync_tasks(
        Arc::clone(&state.git),
        Arc::clone(&state.secrets),
        Arc::clone(&state.venv),
        Arc::clone(&state.project_health),
        projects,
        state.projects_dir.clone(),
        shutdown,
    )
}

// One clone attempt per configured project that has no checkout on disk,
// concurrently, each bounded by its own timeout_seconds (applied inside
// sync_project). A project that is already checked out is left alone: its
// periodic task will pull it, and re-cloning on every reload would throw away
// a working tree for nothing.
fn clone_missing_checkouts(state: Arc<AppState>) -> Vec<tokio::task::JoinHandle<()>> {
    let projects = state.config().projects.clone();
    let mut handles = Vec::new();

    for (project_id, project) in projects {
        let state = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            let checkout = state.projects_dir.join(&project_id).join(".git");
            if tokio::fs::try_exists(&checkout).await.unwrap_or(false) {
                return;
            }
            info!(project = %project_id, "New project — cloning its checkout");
            match sync_project(
                &*state.git,
                &*state.secrets,
                &*state.venv,
                &state.project_health,
                &project_id,
                &project,
                &state.projects_dir,
            )
            .await
            {
                Ok(()) => info!(project = %project_id, "Project checkout ready"),
                Err(e) => error!(project = %project_id, error = %e, "Project sync failed"),
            }
        }));
    }

    handles
}

pub fn start_sync_tasks(
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let config = state.config();

    for (source_id, source) in &config.sources {
        // Config validation rejects schedule + interval together and a
        // schedule that does not parse; the match still has to answer for a
        // Source built without validation (tests), so it fails loudly rather
        // than silently not scheduling.
        let cadence = match (source.schedule.as_deref(), source.sync_interval_seconds) {
            (Some(expression), _) => match parse_cron(expression) {
                Ok(cron) => {
                    info!(source = %source_id, schedule = %expression, "Source scheduled (cron, UTC)");
                    Cadence::Cron(Box::new(cron))
                }
                Err(e) => {
                    error!(source = %source_id, error = %e, "Invalid cron schedule — source will NOT sync");
                    continue;
                }
            },
            (None, Some(secs)) if secs > 0 => {
                info!(source = %source_id, interval_secs = secs, "Source scheduled");
                Cadence::Interval(secs)
            }
            _ => continue,
        };

        // After a panic, an interval task restarts one interval later; a cron
        // task has no interval, so it gets a fixed minute — its own loop then
        // waits for the next scheduled occurrence anyway.
        let restart_delay = match &cadence {
            Cadence::Interval(secs) => Duration::from_secs(*secs),
            Cadence::Cron(_) => Duration::from_secs(60),
        };

        let task_state = Arc::clone(&state);
        let task_source_id = source_id.clone();
        let task_source = source.clone();
        let task_cadence = cadence;
        let task_shutdown = shutdown.clone();

        handles.push(spawn_supervised(
            format!("sync:{}", source_id),
            restart_delay,
            move || {
                let state = Arc::clone(&task_state);
                let source_id = task_source_id.clone();
                let source = task_source.clone();
                let cadence = task_cadence.clone();
                let mut shutdown = task_shutdown.clone();

                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    // Jitter spreads tasks that share an INTERVAL. A cron
                    // schedule is exact times a person chose; shifting those
                    // would be a schedule change, so cron tasks skip it.
                    if let Cadence::Interval(interval_secs) = cadence {
                        tokio::select! {
                            _ = tokio::time::sleep(startup_jitter(&source_id, interval_secs)) => {}
                            _ = shutdown.changed() => return,
                        }
                    }

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
                        tokio::select! {
                            arrived = wait_for_dependency(&*state.cache, &hosts_from.source, budget) => {
                                if !arrived {
                                    warn!(
                                        source = %source_id,
                                        dependency = %hosts_from.source,
                                        waited_seconds = DEPENDENCY_WAIT_SECONDS,
                                        "Source providing the host list has not synced — syncing anyway to record why"
                                    );
                                }
                            }
                            _ = shutdown.changed() => return,
                        }
                    }

                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    match cadence {
                        Cadence::Interval(interval_secs) => {
                            let mut ticker = ticker(interval_secs);
                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {}
                                    _ = shutdown.changed() => return,
                                }
                                if skip > 0 {
                                    skip -= 1;
                                    debug!(source = %source_id, ticks_left = skip, "Backing off, tick skipped");
                                    continue;
                                }
                                scheduled_sync(
                                    &state,
                                    &source_id,
                                    &source,
                                    &mut consecutive_failures,
                                    &mut skip,
                                )
                                .await;
                            }
                        }
                        Cadence::Cron(cron) => loop {
                            // Recomputed per iteration from the wall clock, so
                            // a long sync cannot drift the schedule: the next
                            // occurrence is whatever the expression says it is
                            let Some(delay) = next_cron_delay(&cron, chrono::Utc::now()) else {
                                error!(
                                    source = %source_id,
                                    "Cron schedule has no future occurrence — source will NOT sync again"
                                );
                                return;
                            };
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = shutdown.changed() => return,
                            }
                            if skip > 0 {
                                skip -= 1;
                                debug!(source = %source_id, occurrences_left = skip, "Backing off, occurrence skipped");
                                continue;
                            }
                            scheduled_sync(
                                &state,
                                &source_id,
                                &source,
                                &mut consecutive_failures,
                                &mut skip,
                            )
                            .await;
                        },
                    }
                }
            },
        ));
    }

    handles.extend(start_enricher_tasks(state, shutdown));
    handles
}

// Periodic re-pull of git project checkouts. Separate from start_sync_tasks
// because it doesn't need AppState: main wires it with its own git/secrets
// handles, before the HTTP router even exists.
pub fn start_project_sync_tasks(
    git: Arc<dyn GitPort>,
    secrets: Arc<dyn SecretsPort>,
    venv: Arc<dyn crate::ports::venv::VenvPort>,
    // The same registry instance AppState exposes: main hands it in because
    // these tasks start before the AppState exists
    health: Arc<SyncHealthRegistry>,
    projects: HashMap<String, GitProject>,
    projects_dir: PathBuf,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    for (project_id, project) in projects {
        let cadence = match (project.schedule.as_deref(), project.sync_interval_seconds) {
            (Some(expression), _) => match parse_cron(expression) {
                Ok(cron) => {
                    info!(project = %project_id, schedule = %expression, "Project scheduled (cron, UTC)");
                    Cadence::Cron(Box::new(cron))
                }
                Err(e) => {
                    error!(project = %project_id, error = %e, "Invalid cron schedule — project will NOT re-pull");
                    continue;
                }
            },
            (None, Some(secs)) if secs > 0 => {
                info!(project = %project_id, interval_secs = secs, "Project scheduled");
                Cadence::Interval(secs)
            }
            _ => continue,
        };
        let restart_delay = match &cadence {
            Cadence::Interval(secs) => Duration::from_secs(*secs),
            Cadence::Cron(_) => Duration::from_secs(60),
        };

        let task_git = Arc::clone(&git);
        let task_secrets = Arc::clone(&secrets);
        let task_venv = Arc::clone(&venv);
        let task_health = Arc::clone(&health);
        let task_projects_dir = projects_dir.clone();
        let task_cadence = cadence;
        let task_shutdown = shutdown.clone();

        handles.push(spawn_supervised(
            format!("project:{}", project_id),
            restart_delay,
            move || {
                let git = Arc::clone(&task_git);
                let secrets = Arc::clone(&task_secrets);
                let venv = Arc::clone(&task_venv);
                let health = Arc::clone(&task_health);
                let projects_dir = task_projects_dir.clone();
                let project_id = project_id.clone();
                let project = project.clone();
                let cadence = task_cadence.clone();
                let mut shutdown = task_shutdown.clone();

                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    match cadence {
                        Cadence::Interval(interval_secs) => {
                            tokio::select! {
                                _ = tokio::time::sleep(startup_jitter(&project_id, interval_secs)) => {}
                                _ = shutdown.changed() => return,
                            }
                            let mut ticker = ticker(interval_secs);
                            // The boot sequence already cloned; skip the immediate first tick
                            ticker.tick().await;

                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {}
                                    _ = shutdown.changed() => return,
                                }
                                if skip > 0 {
                                    skip -= 1;
                                    debug!(project = %project_id, ticks_left = skip, "Backing off, tick skipped");
                                    continue;
                                }
                                scheduled_project_pull(
                                    &*git,
                                    &*secrets,
                                    &*venv,
                                    &health,
                                    &project_id,
                                    &project,
                                    &projects_dir,
                                    &mut consecutive_failures,
                                    &mut skip,
                                )
                                .await;
                            }
                        }
                        // No jitter and no first-tick handling for cron: the
                        // first occurrence is in the future by construction,
                        // so the boot clone is never immediately repeated
                        Cadence::Cron(cron) => loop {
                            let Some(delay) = next_cron_delay(&cron, chrono::Utc::now()) else {
                                error!(project = %project_id, "Cron schedule has no future occurrence — project will NOT re-pull");
                                return;
                            };
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = shutdown.changed() => return,
                            }
                            if skip > 0 {
                                skip -= 1;
                                debug!(project = %project_id, occurrences_left = skip, "Backing off, occurrence skipped");
                                continue;
                            }
                            scheduled_project_pull(
                                &*git,
                                &*secrets,
                                &*venv,
                                &health,
                                &project_id,
                                &project,
                                &projects_dir,
                                &mut consecutive_failures,
                                &mut skip,
                            )
                            .await;
                        },
                    }
                }
            },
        ));
    }

    handles
}

fn start_enricher_tasks(
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let config = state.config();

    for (enricher_id, enricher) in &config.enrichers {
        let cadence = match (enricher.schedule.as_deref(), enricher.sync_interval_seconds) {
            (Some(expression), _) => match parse_cron(expression) {
                Ok(cron) => {
                    info!(
                        enricher = %enricher_id,
                        target = %enricher.target_id,
                        schedule = %expression,
                        "Enricher scheduled (cron, UTC)"
                    );
                    Cadence::Cron(Box::new(cron))
                }
                Err(e) => {
                    error!(enricher = %enricher_id, error = %e, "Invalid cron schedule — enricher will NOT run");
                    continue;
                }
            },
            (None, Some(secs)) if secs > 0 => {
                info!(
                    enricher = %enricher_id,
                    target = %enricher.target_id,
                    interval_secs = secs,
                    "Enricher scheduled"
                );
                Cadence::Interval(secs)
            }
            _ => continue,
        };
        let restart_delay = match &cadence {
            Cadence::Interval(secs) => Duration::from_secs(*secs),
            Cadence::Cron(_) => Duration::from_secs(60),
        };

        let task_state = Arc::clone(&state);
        let task_enricher_id = enricher_id.clone();
        let task_enricher = enricher.clone();
        let task_cadence = cadence;
        let task_shutdown = shutdown.clone();

        handles.push(spawn_supervised(
            format!("enrich:{}", enricher_id),
            restart_delay,
            move || {
                let state = Arc::clone(&task_state);
                let enricher_id = task_enricher_id.clone();
                let enricher = task_enricher.clone();
                let cadence = task_cadence.clone();
                let mut shutdown = task_shutdown.clone();

                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    match cadence {
                        Cadence::Interval(interval_secs) => {
                            tokio::select! {
                                _ = tokio::time::sleep(startup_jitter(&enricher_id, interval_secs)) => {}
                                _ = shutdown.changed() => return,
                            }
                            let mut ticker = ticker(interval_secs);

                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {}
                                    _ = shutdown.changed() => return,
                                }
                                if skip > 0 {
                                    skip -= 1;
                                    debug!(enricher = %enricher_id, ticks_left = skip, "Backing off, tick skipped");
                                    continue;
                                }
                                scheduled_enrichment(
                                    &state,
                                    &enricher_id,
                                    &enricher,
                                    &mut consecutive_failures,
                                    &mut skip,
                                )
                                .await;
                            }
                        }
                        Cadence::Cron(cron) => loop {
                            let Some(delay) = next_cron_delay(&cron, chrono::Utc::now()) else {
                                error!(enricher = %enricher_id, "Cron schedule has no future occurrence — enricher will NOT run again");
                                return;
                            };
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = shutdown.changed() => return,
                            }
                            if skip > 0 {
                                skip -= 1;
                                debug!(enricher = %enricher_id, occurrences_left = skip, "Backing off, occurrence skipped");
                                continue;
                            }
                            scheduled_enrichment(
                                &state,
                                &enricher_id,
                                &enricher,
                                &mut consecutive_failures,
                                &mut skip,
                            )
                            .await;
                        },
                    }
                }
            },
        ));
    }

    handles
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
    fn cron_expressions_parse_in_five_and_six_field_forms() {
        parse_cron("0 */2 * * *").expect("standard 5-field");
        parse_cron("*/5 * * * * *").expect("optional leading seconds field");
        parse_cron("whenever feels right").expect_err("junk must not parse");
    }

    #[test]
    fn the_next_cron_delay_is_within_the_expressions_period() {
        let cron = parse_cron("0 * * * *").expect("hourly");
        let delay = next_cron_delay(&cron, chrono::Utc::now()).expect("always a next hour");
        assert!(delay > Duration::ZERO);
        assert!(delay <= Duration::from_secs(3600));
    }

    // A cron source through the whole machinery: the occurrence fires, the
    // sync lands, health records. Every-second cron so real time stays short.
    #[tokio::test]
    async fn a_cron_scheduled_source_syncs_into_the_cache() {
        let source: crate::domain::source::Source = serde_yaml_ng::from_str(concat!(
            "name: cron\n",
            "project_id: p\n",
            "script_path: \"tests/adapters/out/connectors/inventory.py\"\n",
            "ttl_seconds: 3600\n",
            "schedule: \"* * * * * *\"\n",
        ))
        .expect("source fixture");
        let mut sources = HashMap::new();
        sources.insert("src-cron".to_string(), source);
        let (_, state) = crate::AppBuilder::new().sources(sources).build_with_state();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_sync_tasks(Arc::clone(&state), shutdown_rx);
        assert_eq!(
            handles.len(),
            1,
            "a schedule alone must schedule the source"
        );

        let mut synced = false;
        for _ in 0..150 {
            if state.cache.get("src-cron").is_some() {
                synced = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(synced, "the cron occurrence never landed a sync");
        assert_eq!(
            state
                .sync_health
                .get("src-cron")
                .unwrap()
                .consecutive_failures,
            0
        );

        shutdown_tx.send(true).expect("receiver alive");
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("drains")
                .expect("no panic");
        }
    }

    // The same machinery for an enricher: the cron occurrence runs it and the
    // health registry records the success. Declarative merge, so no script
    // process is involved — the test exercises the cadence, not the enricher.
    #[tokio::test]
    async fn a_cron_scheduled_enricher_runs_and_records_health() {
        let enricher: crate::domain::enricher::Enricher = serde_yaml_ng::from_str(concat!(
            "name: cron-merge\n",
            "target_id: src-target\n",
            "source_id: src-extra\n",
            "fields: [\"role\"]\n",
            "schedule: \"* * * * * *\"\n",
        ))
        .expect("enricher fixture");
        let mut enrichers = HashMap::new();
        enrichers.insert("en-cron".to_string(), enricher);
        let (_, state) = crate::AppBuilder::new()
            .enrichers(enrichers)
            .build_with_state();

        // A declarative merge needs both the target and the source cached
        let dataset = || Dataset {
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
        };
        state
            .cache
            .set("src-target", CacheEntry::new(dataset(), 3600));
        state
            .cache
            .set("src-extra", CacheEntry::new(dataset(), 3600));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_sync_tasks(Arc::clone(&state), shutdown_rx);
        assert_eq!(
            handles.len(),
            1,
            "a schedule alone must schedule the enricher"
        );

        let mut ran = false;
        for _ in 0..150 {
            if state.enrich_health.get("en-cron").is_some() {
                ran = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(ran, "the cron occurrence never ran the enricher");
        assert_eq!(
            state
                .enrich_health
                .get("en-cron")
                .unwrap()
                .consecutive_failures,
            0
        );

        shutdown_tx.send(true).expect("receiver alive");
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("drains")
                .expect("no panic");
        }
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

    // The drain contract: every scheduler task returns once the shutdown
    // watch flips — including one still sleeping out its startup jitter or
    // waiting on a tick, which is where a task spends almost all of its life.
    #[tokio::test(start_paused = true)]
    async fn scheduler_tasks_drain_on_the_shutdown_signal() {
        let source: crate::domain::source::Source = serde_yaml_ng::from_str(
            "name: test\nproject_id: p\nscript_path: does-not-exist.py\nschedule: null\nttl_seconds: 60\nsync_interval_seconds: 3600\n",
        )
        .expect("source fixture");
        let mut sources = HashMap::new();
        sources.insert("src-a".to_string(), source);
        let (_, state) = crate::AppBuilder::new().sources(sources).build_with_state();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_sync_tasks(state, shutdown_rx);
        assert_eq!(handles.len(), 1);

        shutdown_tx.send(true).expect("receiver is alive");

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(600), handle)
                .await
                .expect("the task must stop on shutdown, not at its next tick")
                .expect("the task must not panic");
        }
    }

    // The enricher task end to end: the interval fires, the (declarative, so
    // no process spawn) enricher runs, health is recorded, and the task
    // drains. Real time — a 1-second interval plus sub-second jitter.
    #[tokio::test]
    async fn a_scheduled_enricher_runs_and_records_health() {
        let enricher: crate::domain::enricher::Enricher = serde_yaml_ng::from_str(
            "name: e\ntarget_id: src-t\nsource_id: src-s\nfields: [\"f\"]\nsync_interval_seconds: 1\n",
        )
        .expect("enricher fixture");
        let mut enrichers = HashMap::new();
        enrichers.insert("en-a".to_string(), enricher);
        let (_, state) = crate::AppBuilder::new()
            .enrichers(enrichers)
            .build_with_state();
        state
            .cache
            .set("src-t", CacheEntry::new(empty_dataset(), 3600));
        state
            .cache
            .set("src-s", CacheEntry::new(empty_dataset(), 3600));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_sync_tasks(Arc::clone(&state), shutdown_rx);
        assert_eq!(handles.len(), 1, "no sources, one enricher");

        let mut recorded = false;
        for _ in 0..100 {
            if state.enrich_health.get("en-a").is_some() {
                recorded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(recorded, "the scheduled enricher never recorded health");
        assert_eq!(
            state
                .enrich_health
                .get("en-a")
                .unwrap()
                .consecutive_failures,
            0
        );

        shutdown_tx.send(true).expect("receiver alive");
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("drains")
                .expect("no panic");
        }
    }

    // Always-succeeding stand-ins for the project tests below: the scheduler
    // under test only cares WHEN the pull runs, not what git does.
    struct StubGit;
    impl crate::ports::git::GitPort for StubGit {
        fn ensure(
            &self,
            _dir: &std::path::Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> crate::ports::git::GitFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    struct StubVenv;
    impl crate::ports::venv::VenvPort for StubVenv {
        fn ensure(
            &self,
            _projects_dir: &std::path::Path,
            _project_id: &str,
        ) -> crate::ports::venv::VenvFuture<'_> {
            Box::pin(async { Ok(crate::ports::venv::VenvOutcome::NoRequirements) })
        }
    }

    // The project task end to end with a stub git: the boot tick is skipped,
    // the first pull lands at the interval, health is recorded, drain works.
    #[tokio::test]
    async fn a_scheduled_project_pull_runs_and_records_health() {
        let project: GitProject = serde_yaml_ng::from_str(
            "name: p\ngit_url: \"https://example.com/r.git\"\nsync_interval_seconds: 1\n",
        )
        .expect("project fixture");
        let mut projects = HashMap::new();
        projects.insert("prj-a".to_string(), project);

        let health = Arc::new(SyncHealthRegistry::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_project_sync_tasks(
            Arc::new(StubGit),
            Arc::new(crate::adapters::out::secrets::mock::MockSecrets::new()),
            Arc::new(StubVenv),
            Arc::clone(&health),
            projects,
            PathBuf::from("unused"),
            shutdown_rx,
        );
        assert_eq!(handles.len(), 1);

        let mut recorded = false;
        for _ in 0..100 {
            if health.get("prj-a").is_some() {
                recorded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(recorded, "the scheduled pull never recorded health");
        assert_eq!(health.get("prj-a").unwrap().consecutive_failures, 0);

        shutdown_tx.send(true).expect("receiver alive");
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("drains")
                .expect("no panic");
        }
    }

    // The same task on a cron cadence: no boot-tick handling is needed because
    // the first occurrence is in the future by construction, and the pull
    // still lands and records health.
    #[tokio::test]
    async fn a_cron_scheduled_project_pull_runs_and_records_health() {
        let project: GitProject = serde_yaml_ng::from_str(concat!(
            "name: p\n",
            "git_url: \"https://example.com/r.git\"\n",
            "schedule: \"* * * * * *\"\n",
        ))
        .expect("project fixture");
        let mut projects = HashMap::new();
        projects.insert("prj-cron".to_string(), project);

        let health = Arc::new(SyncHealthRegistry::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handles = start_project_sync_tasks(
            Arc::new(StubGit),
            Arc::new(crate::adapters::out::secrets::mock::MockSecrets::new()),
            Arc::new(StubVenv),
            Arc::clone(&health),
            projects,
            PathBuf::from("unused"),
            shutdown_rx,
        );
        assert_eq!(handles.len(), 1, "a schedule alone must schedule the pull");

        let mut recorded = false;
        for _ in 0..150 {
            if health.get("prj-cron").is_some() {
                recorded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(recorded, "the cron occurrence never ran the pull");
        assert_eq!(health.get("prj-cron").unwrap().consecutive_failures, 0);

        shutdown_tx.send(true).expect("receiver alive");
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("drains")
                .expect("no panic");
        }
    }

    // A reload does not reconfigure the running tasks, it replaces them: the
    // generation spawned for the old configuration stops, and a new one runs
    // the new sources. Without this the API would report a reload that had
    // changed nothing about what is actually being gathered.
    #[tokio::test]
    async fn a_reload_replaces_the_running_task_generation() {
        fn source_syncing_every_second() -> crate::domain::source::Source {
            serde_yaml_ng::from_str(concat!(
                "name: fast\n",
                "project_id: p\n",
                "script_path: \"tests/adapters/out/connectors/inventory.py\"\n",
                "ttl_seconds: 3600\n",
                "sync_interval_seconds: 1\n",
            ))
            .expect("source fixture")
        }

        async fn wait_for(state: &AppState, id: &str) -> bool {
            for _ in 0..150 {
                if state.cache.get(id).is_some() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }

        let mut before = HashMap::new();
        before.insert("src-before".to_string(), source_syncing_every_second());
        let (_, state) = crate::AppBuilder::new().sources(before).build_with_state();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let supervisor = start_supervisor(Arc::clone(&state), shutdown_rx);

        assert!(
            wait_for(&state, "src-before").await,
            "the first generation never synced"
        );

        // What a configuration reload does to the state, without the HTTP
        // layer in the way: swap the snapshot, then say so.
        let mut after = HashMap::new();
        after.insert("src-after".to_string(), source_syncing_every_second());
        let previous = state.config();
        state.swap_config(Arc::new(crate::RuntimeConfig {
            sources: after,
            credentials: previous.credentials.clone(),
            views: previous.views.clone(),
            enrichers: previous.enrichers.clone(),
            endpoints: previous.endpoints.clone(),
            projects: previous.projects.clone(),
            secrets: previous.secrets.clone(),
            readyz_require_all_sources: previous.readyz_require_all_sources,
        }));
        state.reload.bump();

        assert!(
            wait_for(&state, "src-after").await,
            "the source that arrived with the reload never got a task"
        );

        // And the outgoing generation really is gone: evict its source and
        // nothing puts it back.
        state.cache.remove("src-before");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            state.cache.get("src-before").is_none(),
            "a task from the previous generation is still syncing a source that is no longer configured"
        );

        shutdown_tx.send(true).expect("receiver alive");
        tokio::time::timeout(Duration::from_secs(10), supervisor)
            .await
            .expect("the supervisor drains its generations")
            .expect("no panic");
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
