use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type OutputResult = Result<String, OutputError>;

#[derive(Debug)]
pub struct OutputError {
    pub message: String,
}

// The script receives the datasets from configured sources on stdin,
// and returns on stdout the format needed by the consumer.
// The response is a raw String — it could be JSON, YAML, CSV, whatever.
pub trait OutputPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        params: &serde_json::Value,
        datasets: &HashMap<String, Dataset>,
    ) -> Pin<Box<dyn Future<Output = OutputResult> + Send + '_>>;
}
