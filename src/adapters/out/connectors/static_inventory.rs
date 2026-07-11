use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tracing::{debug, warn};

use crate::domain::source::OutputFormat;
use crate::domain::static_inventory::{StaticInventoryInput, parse};
use crate::ports::connector::{ConnectorError, ConnectorPort, ConnectorResult};

// Connector for static Ansible YAML inventories: no process is spawned — the
// adapter reads the inventory file plus its sibling group_vars/ and host_vars/
// directories from disk and hands the contents to the domain parser.
//
// `script_path` is reused as "path to the inventory YAML file". With a git
// project, that path resolves inside the checkout (see resolve_script_paths),
// so the periodic project pull — or the on-demand project sync — is what
// refreshes the data the next time this source syncs.
pub struct StaticInventoryConnector;

impl Default for StaticInventoryConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticInventoryConnector {
    pub fn new() -> Self {
        Self
    }
}

impl ConnectorPort for StaticInventoryConnector {
    fn execute(
        &self,
        script_path: &str,
        // No process, no stdout: args and output_format don't apply here
        _args: &[String],
        _output_format: OutputFormat,
        _config: &HashMap<String, String>,
        _credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        let inventory_path = script_path.to_string();

        Box::pin(async move {
            let inventory = tokio::fs::read_to_string(&inventory_path)
                .await
                .map_err(|e| ConnectorError {
                    message: format!("cannot read inventory file '{}': {}", inventory_path, e),
                    stderr: String::new(),
                    exit_code: None,
                })?;

            // group_vars/ and host_vars/ live next to the inventory file,
            // exactly like Ansible resolves them
            let base = Path::new(&inventory_path)
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let group_vars = read_vars_dir(&base.join("group_vars")).await?;
            let host_vars = read_vars_dir(&base.join("host_vars")).await?;

            debug!(
                inventory = %inventory_path,
                group_vars = group_vars.len(),
                host_vars = host_vars.len(),
                "Parsing static inventory"
            );

            let (dataset, warnings) = parse(&StaticInventoryInput {
                inventory,
                group_vars,
                host_vars,
            })
            .map_err(|e| ConnectorError {
                message: format!("static inventory '{}': {}", inventory_path, e),
                stderr: String::new(),
                exit_code: None,
            })?;

            for warning in warnings {
                warn!(inventory = %inventory_path, "{}", warning);
            }

            Ok(dataset)
        })
    }
}

// Read every *.yaml / *.yml in a directory into {name-without-extension:
// contents}. A missing directory is fine (not every inventory has one).
async fn read_vars_dir(dir: &Path) -> Result<HashMap<String, String>, ConnectorError> {
    let mut files = HashMap::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => {
            return Err(ConnectorError {
                message: format!("cannot read '{}': {}", dir.display(), e),
                stderr: String::new(),
                exit_code: None,
            });
        }
    };

    while let Some(entry) = entries.next_entry().await.map_err(|e| ConnectorError {
        message: format!("cannot list '{}': {}", dir.display(), e),
        stderr: String::new(),
        exit_code: None,
    })? {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        if !is_yaml || !path.is_file() {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let contents = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ConnectorError {
                message: format!("cannot read '{}': {}", path.display(), e),
                stderr: String::new(),
                exit_code: None,
            })?;
        files.insert(name, contents);
    }

    Ok(files)
}
