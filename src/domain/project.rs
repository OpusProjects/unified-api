use serde::Deserialize;

// Un GitProject es una referencia a un repo git que contiene
// scripts de connectors y/o transformaciones.
// Un mismo proyecto puede tener múltiples scripts dentro
// (ej: device42/fetch.py, vmware/fetch.py, outputs/format.py)
#[derive(Debug, Deserialize, Clone)]
pub struct GitProject {
    pub name: String,
    pub git_url: String,

    // Rama a clonar/pullear — si no se especifica, "main"
    #[serde(default = "default_branch")]
    pub branch: String,

    // Credencial para repos privados (token de GitHub, SSH key, etc.)
    pub credential_id: Option<String>,

    // Cron para re-pullear el repo periódicamente
    pub sync_interval: Option<String>,
}

// Función que devuelve el valor por defecto para branch.
// #[serde(default = "nombre_funcion")] llama a esta función
// cuando el campo no aparece en el YAML.
fn default_branch() -> String {
    "main".to_string()
}
