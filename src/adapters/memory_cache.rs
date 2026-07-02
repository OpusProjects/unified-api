use dashmap::DashMap;

use crate::domain::cache_entry::CacheEntry;
use crate::ports::cache::CachePort;

// MemoryCache es nuestra implementación concreta de CachePort.
// Usa DashMap: un HashMap concurrente — múltiples threads pueden
// leer y escribir al mismo tiempo sin locks manuales.
pub struct MemoryCache {
    // DashMap<String, CacheEntry> = HashMap<String, CacheEntry> pero thread-safe
    store: DashMap<String, CacheEntry>,
}

impl MemoryCache {
    pub fn new() -> Self {
        Self {
            store: DashMap::new(),
        }
    }
}

// `impl CachePort for MemoryCache` = "MemoryCache cumple el contrato CachePort"
// Es como `class MemoryCache(CachePort):` en Python
// o `class MemoryCache implements CachePort` en Java
impl CachePort for MemoryCache {
    fn set(&self, key: &str, entry: CacheEntry) {
        // .insert() añade o sobreescribe — como dict[key] = value en Python
        self.store.insert(key.to_string(), entry);
    }

    fn get(&self, key: &str) -> Option<CacheEntry> {
        // .get() devuelve Option<Ref<K,V>> — un wrapper de referencia
        // .map(|ref_| ref_.clone()) extrae y clona el valor
        // Es como: return store.get(key, None) en Python, pero con copia
        self.store.get(key).map(|ref_| ref_.clone())
    }

    fn remove(&self, key: &str) {
        self.store.remove(key);
    }

    fn keys(&self) -> Vec<String> {
        // .iter() recorre todos los entries, .map() transforma cada uno,
        // .collect() junta los resultados en un Vec
        // Es como: [entry.key().clone() for entry in store.items()] en Python
        self.store.iter().map(|entry| entry.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dataset::Dataset;
    use std::collections::HashMap;

    fn empty_dataset() -> Dataset {
        Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    #[test]
    fn set_and_get() {
        let cache = MemoryCache::new();
        let entry = CacheEntry::new(empty_dataset(), 3600);

        cache.set("src-1", entry);

        let result = cache.get("src-1");
        assert!(result.is_some()); // is_some() = el Option tiene valor (no es None)
    }

    #[test]
    fn get_missing_key_returns_none() {
        let cache = MemoryCache::new();
        let result = cache.get("no-existe");
        assert!(result.is_none()); // is_none() = el Option es None
    }

    #[test]
    fn remove_deletes_entry() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 3600));

        cache.remove("src-1");

        assert!(cache.get("src-1").is_none());
    }

    #[test]
    fn keys_lists_all_entries() {
        let cache = MemoryCache::new();
        cache.set("src-a", CacheEntry::new(empty_dataset(), 3600));
        cache.set("src-b", CacheEntry::new(empty_dataset(), 3600));

        let mut keys = cache.keys();
        keys.sort(); // sort() ordena in-place (DashMap no garantiza orden)
        assert_eq!(keys, vec!["src-a", "src-b"]); // vec![] crea un Vec literal
    }

    #[test]
    fn set_overwrites_existing() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 100));
        cache.set("src-1", CacheEntry::new(empty_dataset(), 999));

        let entry = cache.get("src-1").unwrap(); // unwrap aquí es seguro, sabemos que existe
        assert_eq!(entry.ttl.as_secs(), 999);
    }
}
