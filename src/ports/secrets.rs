// SecretsPort — the interface every credential is resolved through — lives in
// unified-secrets, shared with unified-gitops. Re-exported here so the ports
// layer still lists every interface the application depends on.
pub use unified_secrets::port::{SecretsError, SecretsFuture, SecretsPort, SecretsSwapPort};
