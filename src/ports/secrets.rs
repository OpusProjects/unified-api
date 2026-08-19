use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

// Alias for the type returned by resolve() — a boxed future (see
// connector.rs for why Pin<Box<dyn Future>>). With the alias, the
// trait and its impls don't repeat the full type each time.
pub type SecretsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HashMap<String, String>, SecretsError>> + Send + 'a>>;

// SecretsPort — interface to resolve credentials from a secrets store
// The concrete implementation will be Vault, but the trait allows testing with mocks
pub trait SecretsPort: Send + Sync {
    // Given a credential_id (e.g., "cred-section9-api"),
    // returns a HashMap with the resolved secrets (e.g., {"username": "admin", "password": "xxx"})
    fn resolve(&self, credential_id: &str) -> SecretsFuture<'_>;
}

#[derive(Debug)]
pub struct SecretsError {
    pub message: String,
}

// Replacing the resolver chain under a running process.
//
// A separate trait from SecretsPort because it is a different direction of
// use: everything in the application RESOLVES secrets, and exactly one thing —
// a configuration reload — replaces the resolver. Keeping them apart means a
// handler cannot swap the chain by accident, and AppState can hold the swap
// capability without naming the adapter that implements it.
pub trait SecretsSwapPort: Send + Sync {
    fn replace(&self, next: std::sync::Arc<dyn SecretsPort>);
}
