use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct OutputEndpoint {
    pub name: String,

    // Qué sources alimentan este endpoint
    #[serde(default)]
    pub source_ids: Vec<String>,

    // Script que transforma los datasets en el formato final
    pub script_path: String,

    // Config libre para el script de transformación
    #[serde(default)]
    pub config: HashMap<String, String>,
}
