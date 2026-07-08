use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;
use crate::ports::cache::CachePort;

// MemoryCache is our concrete implementation of CachePort.
// Uses DashMap: a concurrent HashMap — multiple threads can
// read and write at the same time without manual locks.
pub struct MemoryCache {
    // DashMap<String, CacheEntry> = HashMap<String, CacheEntry> but thread-safe
    store: DashMap<String, CacheEntry>,
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCache {
    pub fn new() -> Self {
        Self {
            store: DashMap::new(),
        }
    }
}

// `impl CachePort for MemoryCache` = "MemoryCache fulfills the CachePort contract"
// It's like `class MemoryCache(CachePort):` in Python
// or `class MemoryCache implements CachePort` in Java
impl CachePort for MemoryCache {
    fn set(&self, key: &str, entry: CacheEntry) {
        // .insert() adds or overwrites — like dict[key] = value in Python
        self.store.insert(key.to_string(), entry);
    }

    fn get(&self, key: &str) -> Option<CacheEntry> {
        // .get() returns Option<Ref<K,V>> — a reference wrapper
        // .map(|ref_| ref_.clone()) extracts and clones the value
        // It's like: return store.get(key, None) in Python, but with a copy
        self.store.get(key).map(|ref_| ref_.clone())
    }

    fn remove(&self, key: &str) {
        self.store.remove(key);
    }

    fn keys(&self) -> Vec<String> {
        // .iter() iterates over all entries, .map() transforms each one,
        // .collect() collects the results into a Vec
        // It's like: [entry.key().clone() for entry in store.items()] in Python
        self.store.iter().map(|entry| entry.key().clone()).collect()
    }

    fn export(&self) -> Vec<(String, CacheEntry)> {
        // Each iteration clones one entry under its shard lock and releases it
        // before the next — the cache is never locked as a whole.
        self.store
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    fn update(&self, key: &str, f: &mut dyn FnMut(&mut CacheEntry)) -> bool {
        // .get_mut() locks the DashMap shard while the guard lives,
        // so `f` modifies the actual entry without anyone else writing.
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
        // The entry API is like Python's dict.setdefault but with a lock:
        // decides "exists / doesn't exist" and acts, all under the same lock.
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
        assert!(result.is_some()); // is_some() = the Option has a value (not None)
    }

    #[test]
    fn get_missing_key_returns_none() {
        let cache = MemoryCache::new();
        let result = cache.get("no-existe");
        assert!(result.is_none()); // is_none() = the Option is None
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
        keys.sort(); // sort() sorts in-place (DashMap does not guarantee order)
        assert_eq!(keys, vec!["src-a", "src-b"]); // vec![] creates a Vec literal
    }

    #[test]
    fn set_overwrites_existing() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 100));
        cache.set("src-1", CacheEntry::new(empty_dataset(), 999));

        let entry = cache.get("src-1").unwrap(); // unwrap here is safe, we know it exists
        assert_eq!(entry.ttl.as_secs(), 999);
    }

    #[test]
    fn update_missing_key_returns_false() {
        let cache = MemoryCache::new();
        let called = cache.update("no-existe", &mut |_entry| {
            panic!("the closure must not be executed if the key does not exist");
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
            panic!("the closure must not be executed if the key does not exist");
        });

        let entry = cache.get("src-1").unwrap();
        assert_eq!(entry.ttl.as_secs(), 123);
    }

    #[test]
    fn merge_or_insert_merges_when_occupied() {
        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(empty_dataset(), 3600));

        let mut partial = empty_dataset();
        partial
            .hostvars
            .insert("host-b".to_string(), HashMap::new());

        cache.merge_or_insert("src-1", partial, 3600, &mut |entry, new| {
            entry.merge_dataset(new);
        });

        let entry = cache.get("src-1").unwrap();
        assert!(entry.dataset.hostvars.contains_key("host-b"));
        // The original TTL is preserved: a new entry was not created
        assert_eq!(entry.ttl.as_secs(), 3600);
    }

    // This test demonstrates the bug that update() fixes: with the old pattern
    // get → modify copy → set, concurrent writers stomp over each other and
    // lose hosts. With update() each write is atomic and nothing is lost.
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
