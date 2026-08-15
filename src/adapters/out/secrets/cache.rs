use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::ports::secrets::{SecretsFuture, SecretsPort};

// A short-TTL cache in front of any SecretsPort.
//
// Resolution happens on EVERY sync of every source (and every enrichment
// re-apply behind it). With EnvSecrets that was free — an env read costs
// nothing — but a secrets backend that is a network call away (Vault) would
// turn the sync schedule into a request storm against it. The TTL is the
// rotation trade, stated in the config docs: a rotated secret is picked up
// within `secrets.cache_ttl_seconds` (default 60); 0 disables the cache, in
// which case main skips this wrapper entirely.
//
// Only SUCCESSES are cached. Errors are retried on the very next resolution —
// negative caching would turn a transient backend blip into ttl_seconds of
// guaranteed failures, and the sync that hits the miss already pays the
// backend's own timeout, not ours.
//
// Two concurrent misses on one id both ask the backend (no in-flight
// coalescing): syncs of one source are already serialized, so concurrent
// misses for the SAME credential are rare, and the price of the race is one
// duplicate read, not a wrong answer.
//
// tokio::time::Instant rather than std: it advances with the paused clock, so
// the expiry tests run in microseconds.
// One cached resolution: when it was fetched and what the backend answered.
struct CacheEntry {
    resolved_at: Instant,
    secrets: HashMap<String, String>,
}

pub struct CachedSecrets {
    inner: Box<dyn SecretsPort>,
    ttl: Duration,
    entries: Mutex<HashMap<String, CacheEntry>>,
}

impl CachedSecrets {
    pub fn new(inner: Box<dyn SecretsPort>, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, credential_id: &str) -> Option<HashMap<String, String>> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .get(credential_id)
            .filter(|entry| entry.resolved_at.elapsed() < self.ttl)
            .map(|entry| entry.secrets.clone())
    }

    fn store(&self, credential_id: &str, secrets: &HashMap<String, String>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(
            credential_id.to_string(),
            CacheEntry {
                resolved_at: Instant::now(),
                secrets: secrets.clone(),
            },
        );
    }
}

impl SecretsPort for CachedSecrets {
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
        let credential_id = credential_id.to_string();

        Box::pin(async move {
            if let Some(secrets) = self.cached(&credential_id) {
                return Ok(secrets);
            }

            let secrets = self.inner.resolve(&credential_id).await?;
            self.store(&credential_id, &secrets);
            Ok(secrets)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::secrets::SecretsError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // An inner port that counts how often it is actually asked.
    struct CountingSecrets {
        calls: std::sync::Arc<AtomicUsize>,
        fail: bool,
    }

    impl SecretsPort for CountingSecrets {
        fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = self.fail;
            let credential_id = credential_id.to_string();
            Box::pin(async move {
                if fail {
                    return Err(SecretsError {
                        message: "backend down".to_string(),
                    });
                }
                Ok(
                    [("token".to_string(), format!("secret-for-{}", credential_id))]
                        .into_iter()
                        .collect(),
                )
            })
        }
    }

    fn counting(fail: bool) -> (CachedSecrets, std::sync::Arc<AtomicUsize>) {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let cache = CachedSecrets::new(
            Box::new(CountingSecrets {
                calls: std::sync::Arc::clone(&calls),
                fail,
            }),
            Duration::from_secs(60),
        );
        (cache, calls)
    }

    #[tokio::test]
    async fn a_fresh_entry_answers_without_asking_the_backend() {
        let (cache, calls) = counting(false);

        let first = cache.resolve("cred-a").await.expect("resolves");
        let second = cache.resolve("cred-a").await.expect("resolves");

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_entry_asks_the_backend_again() {
        let (cache, calls) = counting(false);

        cache.resolve("cred-a").await.expect("resolves");
        tokio::time::advance(Duration::from_secs(61)).await;
        cache.resolve("cred-a").await.expect("resolves");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "past the TTL the backend must be asked again — that is the rotation contract"
        );
    }

    #[tokio::test]
    async fn each_credential_is_cached_separately() {
        let (cache, calls) = counting(false);

        let a = cache.resolve("cred-a").await.expect("resolves");
        let b = cache.resolve("cred-b").await.expect("resolves");

        assert_ne!(a, b);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let (cache, calls) = counting(true);

        cache.resolve("cred-a").await.expect_err("backend fails");
        cache.resolve("cred-a").await.expect_err("backend fails");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a failure must be retried, not remembered for the TTL"
        );
    }
}
