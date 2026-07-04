use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;

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

    // Modificar una entrada existente de forma ATÓMICA.
    //
    // ¿Por qué? get() devuelve una COPIA: el patrón get → modificar → set
    // no es atómico, y dos escritores concurrentes (p.ej. un enricher
    // programado y un PUT de host) se pisan — gana el último set() y las
    // modificaciones del otro se pierden. Aquí el closure `f` recibe una
    // referencia mutable a la entrada REAL, bajo el lock del cache, así que
    // la operación completa leer-modificar-escribir es indivisible.
    //
    // `&mut dyn FnMut(...)` = un closure pasado por referencia. No podemos
    // usar genéricos aquí porque el trait se usa como `dyn CachePort`
    // (object safety: los métodos genéricos no entran en una vtable).
    //
    // Devuelve true si la clave existía (y `f` se ejecutó), false si no.
    // OJO: `f` corre bajo el lock — debe ser rápido y NUNCA llamar al cache.
    fn update(&self, key: &str, f: &mut dyn FnMut(&mut CacheEntry)) -> bool;

    // Fusionar un dataset con la entrada existente, o crear la entrada si
    // no existe — también de forma atómica (misma razón que update()).
    //
    // Si la clave existe: llama a `f(entrada, dataset)` para que el caller
    // decida CÓMO fusionar (merge_dataset, update_group, update_host...).
    // Si no existe: inserta CacheEntry::new(dataset, ttl_seconds) tal cual.
    //
    // El dataset se pasa por valor porque ambas ramas lo consumen, y solo
    // una de las dos puede ejecutarse.
    fn merge_or_insert(
        &self,
        key: &str,
        dataset: Dataset,
        ttl_seconds: u64,
        f: &mut dyn FnMut(&mut CacheEntry, Dataset),
    );
}
