use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{info, warn, error};

use crate::api::sources::resolve_credentials;
use crate::domain::cache_entry::CacheEntry;
use crate::domain::sync_mode::SyncMode;
use crate::AppState;

pub fn start_sync_tasks(state: Arc<AppState>) {
    for (source_id, source) in &state.sources {
        let interval_secs = match source.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue,
        };

        let state = Arc::clone(&state);
        let source_id = source_id.clone();
        let script_path = source.script_path.clone();
        let config = source.config.clone();
        let ttl_seconds = source.ttl_seconds;
        let credential_ids = source.credential_ids.clone();
        let sync_mode = source.sync_mode.clone();

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));

            info!(source = %source_id, interval_secs, "Source scheduled");

            loop {
                ticker.tick().await;
                info!(source = %source_id, "Syncing");

                let temp_source = crate::domain::source::Source {
                    name: String::new(),
                    project_id: String::new(),
                    script_path: script_path.clone(),
                    sync_mode: SyncMode::Replace,
                    credential_ids: credential_ids.clone(),
                    schedule: None,
                    sync_interval_seconds: None,
                    ttl_seconds,
                    ttl_overrides: Default::default(),
                    config: config.clone(),
                };

                let credentials =
                    resolve_credentials(&*state.secrets, &temp_source).await;

                let result = state
                    .connector
                    .execute(&script_path, &config, &credentials)
                    .await;

                match result {
                    Ok(dataset) => {
                        let host_count = dataset.hostvars.len();
                        let group_count = dataset.groups.len();

                        match sync_mode {
                            SyncMode::Replace => {
                                state.cache.set(&source_id, CacheEntry::new(dataset, ttl_seconds));
                            }
                            SyncMode::Merge => {
                                if let Some(mut entry) = state.cache.get(&source_id) {
                                    entry.merge_dataset(dataset);
                                    state.cache.set(&source_id, entry);
                                } else {
                                    state.cache.set(&source_id, CacheEntry::new(dataset, ttl_seconds));
                                }
                            }
                        }

                        info!(source = %source_id, hosts = host_count, groups = group_count, "Synced");
                    }
                    Err(e) => {
                        error!(source = %source_id, error = %e.message, "Sync failed");
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
        let source_id = enricher.source_id.clone();
        let script_path = enricher.script_path.clone();
        let config = enricher.config.clone();

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));

            info!(enricher = %enricher_id, source = %source_id, interval_secs, "Enricher scheduled");

            loop {
                ticker.tick().await;
                info!(enricher = %enricher_id, "Running");

                let current_entry = match state.cache.get(&source_id) {
                    Some(entry) => entry,
                    None => {
                        warn!(enricher = %enricher_id, source = %source_id, "Source not in cache, skipping");
                        continue;
                    }
                };

                let result = state.enricher.execute(
                    &script_path,
                    &config,
                    &current_entry.dataset,
                ).await;

                match result {
                    Ok(partial_dataset) => {
                        let updated = partial_dataset.hostvars.len();
                        let removed = partial_dataset.remove_hosts.len();

                        if let Some(mut entry) = state.cache.get(&source_id) {
                            entry.merge_dataset(partial_dataset);
                            state.cache.set(&source_id, entry);
                        }

                        info!(enricher = %enricher_id, hosts_updated = updated, hosts_removed = removed, "Enriched");
                    }
                    Err(e) => {
                        error!(enricher = %enricher_id, error = %e.message, "Enrichment failed");
                    }
                }
            }
        });
    }
}
