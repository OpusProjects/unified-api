use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;

// A trait is like an interface in Java or a Protocol in Python.
// It defines WHAT operations we need, but not HOW they are done.
//
// Any type that implements this trait can be used as a cache,
// whether DashMap (in-memory), Redis, a file, etc.
//
// Send + Sync = safe to use across threads (necessary for async/axum).
pub trait CachePort: Send + Sync {
    // Store a dataset in cache with a key (the source_id)
    fn set(&self, key: &str, entry: CacheEntry);

    // Retrieve a dataset by key
    // Returns Option: Some(entry) if it exists, None if not
    fn get(&self, key: &str) -> Option<CacheEntry>;

    // Delete a dataset from the cache
    fn remove(&self, key: &str);

    // List all stored keys
    fn keys(&self) -> Vec<String>;

    // Copy out every entry with its key — used by the disk persistence to
    // snapshot the whole cache. Returns owned copies (not references) so the
    // caller can serialize them without holding any cache lock.
    fn export(&self) -> Vec<(String, CacheEntry)>;

    // Modify an existing cache entry atomically.
    //
    // Why? get() returns a COPY: the pattern get → modify → set
    // is not atomic, and two concurrent writers (e.g., a scheduled enricher
    // and a host PUT) can interfere — the last set() wins and the
    // modifications from the other are lost. Here the closure `f` receives a
    // mutable reference to the ACTUAL entry, under the cache's lock, so
    // the complete read-modify-write operation is indivisible.
    //
    // `&mut dyn FnMut(...)` = a closure passed by reference. We cannot
    // use generics here because the trait is used as `dyn CachePort`
    // (object safety: generic methods don't fit in a vtable).
    //
    // Returns true if the key existed (and `f` ran), false if not.
    // NOTE: `f` runs under the lock — it must be fast and NEVER call the cache.
    fn update(&self, key: &str, f: &mut dyn FnMut(&mut CacheEntry)) -> bool;

    // Merge a dataset with an existing entry, or create the entry if
    // it does not exist — also atomically (same reason as update()).
    //
    // If the key exists: calls `f(entry, dataset)` so the caller
    // can decide HOW to merge (merge_dataset, update_group, update_host...).
    // If it does not exist: inserts CacheEntry::new(dataset, ttl_seconds) as-is.
    //
    // The dataset is passed by value because both branches consume it, and only
    // one of the two can execute.
    fn merge_or_insert(
        &self,
        key: &str,
        dataset: Dataset,
        ttl_seconds: u64,
        f: &mut dyn FnMut(&mut CacheEntry, Dataset),
    );
}
