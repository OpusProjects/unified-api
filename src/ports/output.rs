use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
        // CLI arguments for the script (empty slice = none)
        args: &[String],
        config: &HashMap<String, String>,
        params: &serde_json::Value,
        // Arc<Dataset> so handing datasets to an output run shares the cached
        // data instead of deep-copying it (serde's "rc" feature serializes an
        // Arc<T> exactly like a plain T)
        datasets: &HashMap<String, Arc<Dataset>>,
    ) -> Pin<Box<dyn Future<Output = OutputResult> + Send + '_>>;
}
