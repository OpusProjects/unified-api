use dashmap::mapref::entry::Entry;
use dashmap::DashMap;

use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;
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

    fn update(&self, key: &str, f: &mut dyn FnMut(&mut CacheEntry)) -> bool {
        // .get_mut() bloquea el shard del DashMap mientras el guard viva,
        // así que `f` modifica la entrada real sin que nadie más escriba.
        match self.store.get_mut(key) {
            Some(mut guard) => {
                f(guard.value_mut());
                true
            }
            None => false,
        }
    }

    fn merge_or_insert(
        &self,
        key: &str,
        dataset: Dataset,
        ttl_seconds: u64,
        f: &mut dyn FnMut(&mut CacheEntry, Dataset),
    ) {
        // La entry API es como dict.setdefault de Python pero con lock:
        // decide "existe / no existe" y actúa, todo bajo el mismo lock.
        match self.store.entry(key.to_string()) {
            Entry::Occupied(mut occupied) => f(occupied.get_mut(), dataset),
            Entry::Vacant(vacant) => {
                vacant.insert(CacheEntry::new(dataset, ttl_seconds));
            }
        }
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

    #[test]
    fn update_missing_key_returns_false() {
        let cache = MemoryCache::new();
        let called = cache.update("no-existe", &mut |_entry| {
            panic!("el closure no debe ejecutarse si la clave no existe");
        });
        assert!(!called);
    }

    #[test]
    fn update_mutates_entry_in_place() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 3600));

        let called = cache.update("src-1", &mut |entry| {
            entry.update_host("host-a".to_string(), HashMap::new());
        });

        assert!(called);
        let entry = cache.get("src-1").unwrap();
        assert!(entry.dataset.hostvars.contains_key("host-a"));
    }

    #[test]
    fn merge_or_insert_inserts_when_vacant() {
        let cache = MemoryCache::new();

        cache.merge_or_insert("src-1", empty_dataset(), 123, &mut |_entry, _new| {
            panic!("el closure no debe ejecutarse si la clave no existe");
        });

        let entry = cache.get("src-1").unwrap();
        assert_eq!(entry.ttl.as_secs(), 123);
    }

    #[test]
    fn merge_or_insert_merges_when_occupied() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 3600));

        let mut partial = empty_dataset();
        partial.hostvars.insert("host-b".to_string(), HashMap::new());

        cache.merge_or_insert("src-1", partial, 3600, &mut |entry, new| {
            entry.merge_dataset(new);
        });

        let entry = cache.get("src-1").unwrap();
        assert!(entry.dataset.hostvars.contains_key("host-b"));
        // El TTL original se conserva: no se creó una entrada nueva
        assert_eq!(entry.ttl.as_secs(), 3600);
    }

    // Este test demuestra el bug que update() arregla: con el patrón antiguo
    // get → modificar copia → set, escritores concurrentes se pisan y se
    // pierden hosts. Con update() cada escritura es atómica y no se pierde nada.
    #[test]
    fn concurrent_updates_do_not_lose_writes() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(MemoryCache::new());
        cache.set("src-1", CacheEntry::new(empty_dataset(), 3600));

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..50 {
                        cache.update("src-1", &mut |entry| {
                            entry.update_host(format!("host-{}-{}", t, i), HashMap::new());
                        });
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        let entry = cache.get("src-1").unwrap();
        assert_eq!(entry.dataset.hostvars.len(), 8 * 50);
    }
}
