use std::time::Instant;

use crate::domain::enricher::Enricher;
use crate::ports::cache::CachePort;
use crate::ports::enricher::EnricherPort;

// Resultado de ejecutar un enricher — datos puros, sin tipos HTTP
pub struct EnrichOutcome {
    pub hosts_updated: usize,
    pub hosts_removed: usize,
    pub duration_ms: u128,
    pub error: Option<String>,
}

impl EnrichOutcome {
    pub fn success(&self) -> bool {
        self.error.is_none()
    }
}

// El caso de uso "enriquecer un source": ejecutar el script del enricher
// sobre el dataset cacheado y fusionar el resultado parcial.
//
// Devuelve None si el source no está en cache — no hay nada que enriquecer.
pub async fn run_enricher(
    cache: &dyn CachePort,
    enricher_port: &dyn EnricherPort,
    enricher: &Enricher,
) -> Option<EnrichOutcome> {
    // Snapshot de lectura: el enricher ejecuta un script y tarda, no podemos
    // retener el lock del cache mientras corre. El merge de abajo sí es
    // atómico, así que las escrituras concurrentes durante la ejecución no
    // se pierden (el enricher pisa solo los hosts que él mismo devuelve).
    let current_entry = cache.get(&enricher.source_id)?;

    let start = Instant::now();

    let result = enricher_port
        .execute(&enricher.script_path, &enricher.config, &current_entry.dataset)
        .await;

    let duration_ms = start.elapsed().as_millis();

    Some(match result {
        Ok(partial_dataset) => {
            let hosts_updated = partial_dataset.hostvars.len();
            let hosts_removed = partial_dataset.remove_hosts.len();

            // Option::take dentro del closure: merge_dataset consume el
            // dataset pero un FnMut no puede mover lo que captura
            let mut partial = Some(partial_dataset);
            cache.update(&enricher.source_id, &mut |entry| {
                if let Some(p) = partial.take() {
                    entry.merge_dataset(p);
                }
            });

            EnrichOutcome {
                hosts_updated,
                hosts_removed,
                duration_ms,
                error: None,
            }
        }
        Err(e) => EnrichOutcome {
            hosts_updated: 0,
            hosts_removed: 0,
            duration_ms,
            error: Some(e.message),
        },
    })
}
