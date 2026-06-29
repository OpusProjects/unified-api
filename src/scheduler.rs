use std::sync::Arc;
use tokio::time::{interval, Duration};

use crate::api::sources::resolve_credentials;
use crate::domain::cache_entry::CacheEntry;
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

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));

            println!(
                "[scheduler] Source '{}' scheduled every {}s",
                source_id, interval_secs
            );

            loop {
                ticker.tick().await;

                println!("[scheduler] Syncing '{}'...", source_id);

                // Resolver credenciales desde Vault/mock antes de ejecutar
                // Construimos un Source temporal para reutilizar resolve_credentials
                let temp_source = crate::domain::source::Source {
                    name: String::new(),
                    project_id: String::new(),
                    script_path: script_path.clone(),
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
                        state
                            .cache
                            .set(&source_id, CacheEntry::new(dataset, ttl_seconds));
                        println!(
                            "[scheduler] '{}' synced: {} hosts, {} groups",
                            source_id, host_count, group_count
                        );
                    }
                    Err(e) => {
                        println!(
                            "[scheduler] '{}' sync failed: {}",
                            source_id, e.message
                        );
                    }
                }
            }
        });
    }
}
