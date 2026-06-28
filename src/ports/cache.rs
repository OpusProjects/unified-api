use crate::domain::cache_entry::CacheEntry;

// Un trait es como una interfaz en Java o un Protocol en Python.
// Define QUÉ operaciones necesitamos, pero no CÓMO se hacen.
//
// Cualquier tipo que implemente este trait se puede usar como caché,
// ya sea DashMap (memoria), Redis, un archivo, etc.
//
// Send + Sync = seguros para usar entre threads (necesario para async/axum).
pub trait CachePort: Send + Sync {
    // Guardar un dataset en caché con una clave (el source_id)
    fn set(&self, key: &str, entry: CacheEntry);

    // Recuperar un dataset por clave
    // Devuelve Option: Some(entry) si existe, None si no
    fn get(&self, key: &str) -> Option<CacheEntry>;

    // Borrar un dataset de la caché
    fn remove(&self, key: &str);

    // Listar todas las claves almacenadas
    fn keys(&self) -> Vec<String>;
}
