use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Un Dataset es un inventario Ansible completo, tal como lo produce un connector.
// Serialize + Deserialize: puede convertirse a/desde JSON/YAML en ambas direcciones.
// Clone: permite hacer copias del struct (Rust por defecto mueve, no copia).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Dataset {
    // HashMap<String, HostVars> = dict[str, HostVars] en Python
    // Mapea nombre de host → sus variables
    #[serde(default)]
    pub hostvars: HashMap<String, HostVars>,

    // HashMap<String, Group> = los grupos del inventario (dc06, oraclelinux8, etc.)
    #[serde(default)]
    pub groups: HashMap<String, Group>,
}

// Las variables de un host: un diccionario libre de clave-valor
// serde_json::Value es como "any" — puede ser string, número, bool, lista, etc.
pub type HostVars = HashMap<String, serde_json::Value>;

// Un grupo del inventario Ansible
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Group {
    // Vec<String> = list[str] en Python
    // #[serde(default)] = si no viene en el YAML/JSON, usa un Vec vacío
    #[serde(default)]
    pub hosts: Vec<String>,

    #[serde(default)]
    pub children: Vec<String>,

    // Option<HostVars> = puede existir o no (como Optional en Python)
    // None = no tiene variables de grupo, Some({...}) = sí tiene
    #[serde(default)]
    pub vars: Option<HostVars>,
}
