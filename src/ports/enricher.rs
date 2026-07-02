use crate::domain::dataset::Dataset;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type EnricherResult = Result<Dataset, EnricherError>;

#[derive(Debug)]
pub struct EnricherError {
    pub message: String,
}

// Un enricher recibe el dataset actual y devuelve un dataset parcial
// con los hosts modificados y/o hosts a eliminar
pub trait EnricherPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        config: &HashMap<String, String>,
        current_dataset: &Dataset,
    ) -> Pin<Box<dyn Future<Output = EnricherResult> + Send + '_>>;
}
