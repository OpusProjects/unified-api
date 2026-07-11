use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type EnricherResult = Result<Dataset, EnricherError>;

#[derive(Debug)]
pub struct EnricherError {
    pub message: String,
}

// An enricher receives the current dataset and returns a partial dataset
// with modified hosts and/or hosts to remove
pub trait EnricherPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        // CLI arguments for the script (empty slice = none)
        args: &[String],
        config: &HashMap<String, String>,
        current_dataset: &Dataset,
    ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>>;
}
