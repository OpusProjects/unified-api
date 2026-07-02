use serde::Deserialize;

// Cómo se aplican los datos al cache cuando llegan
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    // Borra todo y pone lo nuevo — el script trae el inventario completo
    #[default]
    Replace,
    // Parchea solo lo que viene — el resto no se toca
    Merge,
}
