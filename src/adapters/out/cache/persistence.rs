use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::Dataset;
use crate::ports::cache::CachePort;

// Disk persistence for the in-memory cache: an OPTIONAL snapshot file.
//
// The cache stays the source of truth and DashMap keeps serving reads and
// writes exactly as before — this module only copies it to disk every
// `interval_seconds` and reloads it at boot, so a restart starts from the
// last snapshot instead of an empty cache (and /readyz is green immediately).
//
// CacheEntry holds `Instant`s, which are process-relative and cannot be
// written to disk. The snapshot therefore stores AGES (how many seconds old
// each thing was when the snapshot was taken); loading converts them back to
// Instants relative to the new process (see CacheEntry::restore).

// One snapshot file = one SnapshotFile. `version` lets a future format change
// detect (and skip) files written by an older binary instead of misparsing them.
#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    entries: HashMap<String, SnapshotEntry>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotEntry {
    dataset: Dataset,
    ttl_seconds: u64,
    age_seconds: u64,
    host_ages: HashMap<String, u64>,
}

const SNAPSHOT_VERSION: u32 = 1;

// Serialize the whole cache and write it to `path` atomically: write to a
// sibling temp file first, then rename over the target. rename() on the same
// filesystem is atomic in POSIX, so a crash mid-write leaves the previous
// snapshot intact instead of a truncated JSON file.
pub async fn save(cache: &dyn CachePort, path: &Path) -> Result<usize, String> {
    let entries: HashMap<String, SnapshotEntry> = cache
        .export()
        .into_iter()
        .map(|(key, entry)| {
            let host_ages = entry
                .dataset
                .hostvars
                .keys()
                .filter_map(|host| entry.host_age_seconds(host).map(|age| (host.clone(), age)))
                .collect();
            (
                key,
                SnapshotEntry {
                    ttl_seconds: entry.ttl.as_secs(),
                    age_seconds: entry.age_seconds(),
                    host_ages,
                    dataset: entry.dataset,
                },
            )
        })
        .collect();

    let count = entries.len();
    let snapshot = SnapshotFile {
        version: SNAPSHOT_VERSION,
        entries,
    };

    let json = serde_json::to_vec(&snapshot).map_err(|e| format!("serialize snapshot: {}", e))?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create snapshot directory '{}': {}", parent.display(), e))?;
    }

    let tmp: PathBuf = path.with_extension("tmp");
    tokio::fs::write(&tmp, &json)
        .await
        .map_err(|e| format!("write '{}': {}", tmp.display(), e))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| format!("rename '{}' to '{}': {}", tmp.display(), path.display(), e))?;

    Ok(count)
}

// Load a snapshot file into the cache. Missing file is NOT an error — it is
// simply the first boot (or persistence was just enabled): start empty.
pub async fn load(cache: &dyn CachePort, path: &Path) -> Result<usize, String> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("read '{}': {}", path.display(), e)),
    };

    let snapshot: SnapshotFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse '{}': {}", path.display(), e))?;

    if snapshot.version != SNAPSHOT_VERSION {
        return Err(format!(
            "snapshot '{}' has version {} but this binary writes version {} — ignoring it",
            path.display(),
            snapshot.version,
            SNAPSHOT_VERSION
        ));
    }

    let count = snapshot.entries.len();
    for (key, entry) in snapshot.entries {
        cache.set(
            &key,
            CacheEntry::restore(
                entry.dataset,
                entry.ttl_seconds,
                entry.age_seconds,
                entry.host_ages,
            ),
        );
    }

    Ok(count)
}

// Boot-time load with logging: called from main. A corrupt or unreadable
// snapshot must not prevent startup — the cache just starts empty and the
// schedulers repopulate it, exactly like before persistence existed.
pub async fn load_or_warn(cache: &dyn CachePort, path: &Path) {
    match load(cache, path).await {
        Ok(0) => info!(path = %path.display(), "No cache snapshot found, starting empty"),
        Ok(count) => info!(path = %path.display(), entries = count, "Cache snapshot loaded"),
        Err(e) => warn!(path = %path.display(), error = %e, "Ignoring cache snapshot"),
    }
}

// Spawn the periodic snapshot task. The first tick of tokio's interval fires
// immediately, so we skip it — there is nothing worth saving at boot beyond
// what was just loaded.
pub fn start_snapshot_task(cache: Arc<dyn CachePort>, path: PathBuf, interval_seconds: u64) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds));
        ticker.tick().await;

        info!(path = %path.display(), interval_seconds, "Cache persistence scheduled");

        loop {
            ticker.tick().await;
            match save(&*cache, &path).await {
                Ok(count) => {
                    tracing::debug!(path = %path.display(), entries = count, "Cache snapshot saved");
                }
                Err(e) => error!(path = %path.display(), error = %e, "Cache snapshot failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::dataset::Group;

    fn dataset() -> Dataset {
        Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("commander"))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: [(
                "section9".to_string(),
                Group {
                    hosts: vec!["motoko.section9.net".to_string()],
                    children: vec![],
                    vars: None,
                },
            )]
            .into_iter()
            .collect(),
            remove_hosts: vec![],
        }
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(dataset(), 3600));

        let saved = save(&cache, &path).await.unwrap();
        assert_eq!(saved, 1);

        let restored = MemoryCache::new();
        let loaded = load(&restored, &path).await.unwrap();
        assert_eq!(loaded, 1);

        let entry = restored.get("src-1").unwrap();
        assert_eq!(entry.ttl.as_secs(), 3600);
        assert!(entry.is_fresh());
        assert!(entry.is_host_fresh("motoko.section9.net", None));
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["role"],
            "commander"
        );
        assert_eq!(
            entry.dataset.groups["section9"].hosts,
            vec!["motoko.section9.net"]
        );
    }

    #[tokio::test]
    async fn expired_entries_stay_expired_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let cache = MemoryCache::new();
        // TTL 0 = expired from the moment it was created
        cache.set("src-1", CacheEntry::new(dataset(), 0));
        tokio::time::sleep(Duration::from_millis(10)).await;

        save(&cache, &path).await.unwrap();

        let restored = MemoryCache::new();
        load(&restored, &path).await.unwrap();

        // The data survives the restart, but its freshness does not reset
        let entry = restored.get("src-1").unwrap();
        assert!(!entry.is_fresh());
    }

    #[tokio::test]
    async fn missing_file_loads_zero_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MemoryCache::new();
        let loaded = load(&cache, &dir.path().join("nope.json")).await.unwrap();
        assert_eq!(loaded, 0);
        assert!(cache.keys().is_empty());
    }

    #[tokio::test]
    async fn corrupt_file_returns_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        tokio::fs::write(&path, b"not json at all").await.unwrap();

        let cache = MemoryCache::new();
        assert!(load(&cache, &path).await.is_err());
    }

    #[tokio::test]
    async fn unknown_snapshot_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        tokio::fs::write(&path, br#"{"version": 999, "entries": {}}"#)
            .await
            .unwrap();

        let cache = MemoryCache::new();
        assert!(load(&cache, &path).await.is_err());
    }

    #[tokio::test]
    async fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/cache.json");

        let cache = MemoryCache::new();
        cache.set("src-1", CacheEntry::new(dataset(), 60));

        save(&cache, &path).await.unwrap();
        assert!(path.exists());
    }
}
