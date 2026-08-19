use std::sync::{Arc, RwLock};

use crate::ports::secrets::{SecretsFuture, SecretsPort, SecretsSwapPort};

// The resolver chain, behind one pointer that a reload can replace.
//
// Everything else in the process holds this, not the chain underneath it, so
// changing credentials.yaml (or the whole `secrets:` block, Vault included)
// means building a fresh chain and swapping it here — no restart, and no
// caller anywhere that has to be told.
//
// A resolution already in flight keeps the chain it started with: it holds an
// Arc, and the swap only changes what the NEXT resolution reads.
pub struct ReloadableSecrets {
    inner: RwLock<Arc<dyn SecretsPort>>,
}

impl ReloadableSecrets {
    pub fn new(inner: Arc<dyn SecretsPort>) -> Self {
        Self {
            inner: RwLock::new(inner),
        }
    }

    fn current(&self) -> Arc<dyn SecretsPort> {
        Arc::clone(&self.inner.read().expect("secrets chain lock"))
    }
}

impl SecretsPort for ReloadableSecrets {
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_> {
        let chain = self.current();
        let credential_id = credential_id.to_string();
        // The chain is resolved BEFORE the future is awaited and moved into
        // it, so a swap landing mid-resolution cannot pull it out from under
        // a request that already started.
        Box::pin(async move { chain.resolve(&credential_id).await })
    }
}

impl SecretsSwapPort for ReloadableSecrets {
    fn replace(&self, next: Arc<dyn SecretsPort>) {
        *self.inner.write().expect("secrets chain lock") = next;
    }
}
