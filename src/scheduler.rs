use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{interval, Duration};

use crate::domain::cache_entry::CacheEntry;
use crate::AppState;

// Lanza un tokio::spawn por cada source que tenga sync_interval_seconds > 0
// tokio::spawn = lanza una tarea asíncrona en background (como un thread ligero)
// Cada tarea ejecuta un loop infinito: espera el intervalo → ejecuta sync → repite
pub fn start_sync_tasks(state: Arc<AppState>) {
    for (source_id, source) in &state.sources {
        let interval_secs = match source.sync_interval_seconds {
            Some(secs) if secs > 0 => secs,
            _ => continue, // sin intervalo = no se programa
        };

        // Clonamos lo que el task necesita — cada spawn es independiente
        // y necesita ser dueño de sus datos (ownership)
        let state = Arc::clone(&state);
        let source_id = source_id.clone();
        let script_path = source.script_path.clone();
        let config = source.config.clone();
        let ttl_seconds = source.ttl_seconds;

        // tokio::spawn lanza la tarea y devuelve inmediatamente
        // El task vive mientras el runtime de tokio viva (= mientras el server corra)
        tokio::spawn(async move {
            // interval() crea un ticker que dispara cada X duración
            // El primer tick es inmediato
            let mut ticker = interval(Duration::from_secs(interval_secs));

            println!(
                "[scheduler] Source '{}' scheduled every {}s",
                source_id, interval_secs
            );

            loop {
                ticker.tick().await; // espera hasta el siguiente tick

                println!("[scheduler] Syncing '{}'...", source_id);

                let credentials: HashMap<String, String> = HashMap::new();
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
