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
