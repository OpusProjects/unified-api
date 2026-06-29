use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type ConnectorResult = Result<Dataset, ConnectorError>;

#[derive(Debug)]
pub struct ConnectorError {
    pub message: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

// Para que un trait sea compatible con `dyn` (dispatch dinámico),
// los métodos async necesitan devolver un tipo concreto, no `impl Future`.
// `Pin<Box<dyn Future>>` es la forma estándar de hacer esto:
// - Box: el Future se guarda en el heap (porque no sabemos su tamaño)
// - Pin: el Future no se puede mover en memoria (requisito de async en Rust)
// Es un poco feo, pero es el patrón estándar para async traits con dyn.
pub trait ConnectorPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>>;
}
