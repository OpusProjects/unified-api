use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

use crate::AppState;
use crate::application::enrich::run_enricher;
use crate::application::projects::sync_project;
use crate::application::sync::{SyncScope, sync_source};
use crate::domain::project::GitProject;
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;

// The scheduler is another "driving adapter", just like HTTP handlers:
// it triggers the same use cases from application/, just by time instead
// of by request. There is no business logic here — only timers and logs.
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
            let mut ticker = interval(Duration::from_secs(interval_secs));

            info!(source = %source_id, interval_secs, "Source scheduled");

            loop {
                ticker.tick().await;
                info!(source = %source_id, "Syncing");

                let connector = state.connector_for(&source.connector_type);
                let outcome = sync_source(
                    &*state.cache,
                    &**connector,
                    &*state.secrets,
                    &source_id,
                    &source,
                    SyncScope::Full,
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
        let projects_dir = projects_dir.clone();

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));
            // The boot sequence already cloned; skip the immediate first tick
            ticker.tick().await;

            info!(project = %project_id, interval_secs, "Project scheduled");

            loop {
                ticker.tick().await;
                match sync_project(&*git, &*secrets, &project_id, &project, &projects_dir).await {
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
            let mut ticker = interval(Duration::from_secs(interval_secs));

            info!(
                enricher = %enricher_id,
                source = %enricher.source_id,
                interval_secs,
                "Enricher scheduled"
            );

            loop {
                ticker.tick().await;
                info!(enricher = %enricher_id, "Running");

                match run_enricher(&*state.cache, &*state.enricher, &enricher).await {
                    None => {
                        warn!(
                            enricher = %enricher_id,
                            source = %enricher.source_id,
                            "Source not in cache, skipping"
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
