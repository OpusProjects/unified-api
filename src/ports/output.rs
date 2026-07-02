use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type OutputResult = Result<String, OutputError>;

#[derive(Debug)]
pub struct OutputError {
    pub message: String,
}

// El script recibe los datasets de los sources configurados por stdin,
// y devuelve por stdout el formato que necesita el consumidor.
// La respuesta es un String crudo — puede ser JSON, YAML, CSV, lo que sea.
pub trait OutputPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        params: &serde_json::Value,
        datasets: &HashMap<String, Dataset>,
    ) -> Pin<Box<dyn Future<Output = OutputResult> + Send + '_>>;
}
