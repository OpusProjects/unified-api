use serde::Deserialize;
use std::collections::HashMap;

// Scope de consulta: qué porción del inventario quiere el consumidor
// Se usa como query param: ?scope=full, ?scope=group&name=dc06, ?scope=host&name=host01
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum InventoryScope {
    Full,
    Group,
    Host,
}

// Un OutputEndpoint define un endpoint de la API que transforma
// y sirve datos de uno o más Sources para un consumidor específico
// (AnsibleForms, AWX, herramientas custom, etc.)
#[derive(Debug, Deserialize, Clone)]
pub struct OutputEndpoint {
    pub name: String,

    // La ruta URL donde se sirve, ej: "/api/v1/inventory/linux-full"
    pub url_path: String,

    // Qué sources alimentan este endpoint
    #[serde(default)]
    pub source_ids: Vec<String>,

    // Git project + script que transforma los datasets en el formato final
    pub project_id: Option<String>,
    pub script_path: Option<String>,

    // Credenciales si la transformación necesita acceso externo
    #[serde(default)]
    pub credential_ids: Vec<String>,

    // Config libre para el script de transformación
    #[serde(default)]
    pub config: HashMap<String, String>,
}
