use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

// Why a source's data looks the way it does. The cache entry says how OLD the
// data is; this says whether anything is still successfully refreshing it, and
// what went wrong if not. Without it a failing connector is indistinguishable
// from a long sync interval: both look like a dataset slowly getting older.
//
// Ages rather than wall-clock timestamps, because that is what the rest of the
// API reports (age_seconds everywhere) and because Instant is monotonic — it
// cannot be dragged backwards by an NTP correction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncHealth {
    /// Seconds since a sync was last attempted, successful or not
    pub last_attempt_age_seconds: Option<u64>,
    /// Seconds since the last sync that succeeded
    pub last_success_age_seconds: Option<u64>,
    /// The error from the most recent attempt, if it failed
    pub last_error: Option<String>,
    /// Failed attempts since the last success — 0 while healthy
    pub consecutive_failures: u32,
}

// What is stored internally: Instants, converted to ages when read.
#[derive(Debug)]
struct Record {
    last_attempt: Instant,
    last_success: Option<Instant>,
    last_error: Option<String>,
    consecutive_failures: u32,
}

// Shared, mutable, keyed by source id.
//
// It lives outside the cache on purpose: the interesting case is a source with
// NO cache entry — one that has never synced successfully — and a registry
// keyed independently keeps that record instead of having nowhere to put it.
//
// A std Mutex rather than the DashMap used by the cache: writes happen once per
// sync attempt (seconds apart at best), so there is no contention to design
// for, and this keeps the domain layer free of extra dependencies.
#[derive(Debug, Default)]
pub struct SyncHealthRegistry {
    inner: Mutex<HashMap<String, Record>>,
}

impl SyncHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&self, source_id: &str) {
        let now = Instant::now();
        self.with_map(|map| {
            map.insert(
                source_id.to_string(),
                Record {
                    last_attempt: now,
                    last_success: Some(now),
                    last_error: None,
                    consecutive_failures: 0,
                },
            );
        });
    }

    pub fn record_failure(&self, source_id: &str, error: &str) {
        let now = Instant::now();
        self.with_map(|map| {
            // A failure keeps the last success: "last worked 6 hours ago,
            // failing since" is the whole point.
            let entry = map.entry(source_id.to_string()).or_insert(Record {
                last_attempt: now,
                last_success: None,
                last_error: None,
                consecutive_failures: 0,
            });
            entry.last_attempt = now;
            entry.last_error = Some(error.to_string());
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        });
    }

    pub fn get(&self, source_id: &str) -> Option<SyncHealth> {
        let now = Instant::now();
        self.with_map(|map| {
            map.get(source_id).map(|record| SyncHealth {
                last_attempt_age_seconds: Some(now.duration_since(record.last_attempt).as_secs()),
                last_success_age_seconds: record
                    .last_success
                    .map(|at| now.duration_since(at).as_secs()),
                last_error: record.last_error.clone(),
                consecutive_failures: record.consecutive_failures,
            })
        })
    }

    // A panic elsewhere while this lock was held must not take the API down:
    // health data is a side-channel for humans, so recovering the map from a
    // poisoned lock is strictly better than propagating the panic into every
    // later request.
    fn with_map<T>(&self, f: impl FnOnce(&mut HashMap<String, Record>) -> T) -> T {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_source_has_no_health() {
        let registry = SyncHealthRegistry::new();
        assert_eq!(registry.get("src-nope"), None);
    }

    #[test]
    fn success_clears_the_error_and_the_failure_count() {
        let registry = SyncHealthRegistry::new();

        registry.record_failure("src-a", "connection refused");
        registry.record_failure("src-a", "connection refused");
        let health = registry.get("src-a").unwrap();
        assert_eq!(health.consecutive_failures, 2);
        assert_eq!(health.last_error.as_deref(), Some("connection refused"));
        assert_eq!(health.last_success_age_seconds, None);

        registry.record_success("src-a");
        let health = registry.get("src-a").unwrap();
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.last_error, None);
        assert!(health.last_success_age_seconds.is_some());
    }

    #[test]
    fn failure_after_success_keeps_the_last_success() {
        let registry = SyncHealthRegistry::new();

        registry.record_success("src-a");
        registry.record_failure("src-a", "script exited 1");

        let health = registry.get("src-a").unwrap();
        // "last worked N seconds ago, failing since" — the reason a failure
        // must not wipe the success timestamp
        assert!(health.last_success_age_seconds.is_some());
        assert_eq!(health.last_error.as_deref(), Some("script exited 1"));
        assert_eq!(health.consecutive_failures, 1);
    }

    #[test]
    fn each_source_is_tracked_separately() {
        let registry = SyncHealthRegistry::new();

        registry.record_failure("src-a", "boom");
        registry.record_success("src-b");

        assert_eq!(registry.get("src-a").unwrap().consecutive_failures, 1);
        assert_eq!(registry.get("src-b").unwrap().consecutive_failures, 0);
    }
}
