use serde::Deserialize;
use std::collections::HashMap;

// Un Enricher post-procesa datos que ya están en cache.
// Recibe el dataset actual de un source, lo transforma, y devuelve
// los hosts modificados (merge) y/o hosts a eliminar.
#[derive(Debug, Deserialize, Clone)]
pub struct Enricher {
    pub name: String,

    // El source cuyos datos enriquece — ej: "src-device42"
    pub source_id: String,

    // Script que ejecuta el enriquecimiento
    pub script_path: String,

    // Intervalo de ejecución automática (0 o None = solo manual)
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Config libre para el script
    #[serde(default)]
    pub config: HashMap<String, String>,
}
