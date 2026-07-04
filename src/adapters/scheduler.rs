use std::sync::Arc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

use crate::AppState;
use crate::application::enrich::run_enricher;
use crate::application::sync::{SyncScope, sync_source};

// El scheduler es un "driving adapter" más, igual que los handlers HTTP:
// dispara los mismos casos de uso de application/, solo que por tiempo en
// vez de por request. Aquí no hay lógica de negocio — solo timers y logs.
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
