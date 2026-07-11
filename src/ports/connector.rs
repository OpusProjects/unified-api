use crate::domain::dataset::Dataset;
use crate::domain::source::OutputFormat;
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

// For a trait to be compatible with `dyn` (dynamic dispatch),
// async methods need to return a concrete type, not `impl Future`.
// `Pin<Box<dyn Future>>` is the standard way to do this:
// - Box: the Future is stored on the heap (because we don't know its size)
// - Pin: the Future cannot move in memory (a requirement for async in Rust)
// It's a bit ugly, but it's the standard pattern for async traits with dyn.
pub trait ConnectorPort: Send + Sync {
    fn execute(
        &self,
        script_path: &str,
        // CLI arguments for the script — many inventory scripts follow the
        // Ansible dynamic inventory convention and require e.g. `--list`
        args: &[String],
        // How to interpret the script's stdout (native Dataset vs Ansible
        // inventory JSON). The SSH connector builds its Dataset itself and
        // ignores this.
        output_format: OutputFormat,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>>;
}
