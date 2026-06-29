use serde::Deserialize;
use std::collections::HashMap;

// Un Source define cómo ingestar datos de una fuente externa
// Es lo que se configura en config.yaml bajo "sources:"
#[derive(Debug, Deserialize, Clone)]
pub struct Source {
    pub name: String,
    pub project_id: String,
    pub script_path: String,

    // Vec<String> = lista de IDs de credenciales que necesita el connector
    #[serde(default)]
    pub credential_ids: Vec<String>,

    // Option<String> = puede tener o no un schedule cron (reservado para futuro)
    pub schedule: Option<String>,

    // Intervalo de sync automático en segundos (alternativa simple al cron)
    // Si es None o 0, no se hace sync automático
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // TTL en segundos para la caché de este source
    pub ttl_seconds: u64,

    // Overrides de TTL por grupo o por host
    #[serde(default)]
    pub ttl_overrides: TtlOverrides,

    // Configuración libre para el connector (api_url, filters, etc.)
    #[serde(default)]
    pub config: HashMap<String, String>,
}

// Overrides de TTL: puedes dar TTLs distintos a grupos o hosts específicos
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TtlOverrides {
    // HashMap<String, u64> = dict[str, int] en Python
    // ej: {"production": 900} → el grupo "production" refresca cada 15 min
    #[serde(default)]
    pub groups: HashMap<String, u64>,

    // ej: {"critical-db01": 300} → este host refresca cada 5 min
    #[serde(default)]
    pub hosts: HashMap<String, u64>,
}
