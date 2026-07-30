use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
        // Arc rather than &Dataset: the returned future must own what it reads,
        // so a borrow left every adapter no choice but to deep-copy the dataset
        // — on a facts source that is megabytes of HashMaps cloned per run, per
        // enricher. The cache already holds this behind an Arc (see CacheEntry),
        // so passing it on is a refcount bump and the copy disappears. Same
        // reasoning, and the same signature, as OutputPort.
        current_dataset: Arc<Dataset>,
    ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>>;
}
