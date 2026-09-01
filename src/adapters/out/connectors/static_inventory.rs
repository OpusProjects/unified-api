use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tracing::{debug, warn};

use crate::domain::source::OutputFormat;
use crate::domain::static_inventory::{StaticInventoryInput, parse};

// name → the files that define it, in merge order: (label for errors, contents)
type VarsFiles = HashMap<String, Vec<(String, String)>>;
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

            Ok(dataset.into())
        })
    }
}

// Read a group_vars/ or host_vars/ directory into {name → the files that
// define it}. A missing directory is fine (not every inventory has one).
//
// Ansible accepts either layout, and so does this: `group_vars/web.yaml`, or
// `group_vars/web/` holding any number of files that are merged together. The
// directory form is how a large inventory stays readable — one file per
// concern rather than one enormous file per group — and reading only the flat
// form dropped every variable of an inventory written the other way. Silently:
// the sync reported every host and every group, each with no vars at all.
async fn read_vars_dir(dir: &Path) -> Result<VarsFiles, ConnectorError> {
    let mut files: VarsFiles = HashMap::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(read_error(dir, e)),
    };

    while let Some(entry) = entries.next_entry().await.map_err(|e| read_error(dir, e))? {
        let path = entry.path();

        let (name, parts) = if path.is_dir() {
            let Some(name) = file_name(&path) else {
                continue;
            };
            (name, read_vars_subdir(&path).await?)
        } else {
            if !is_yaml(&path) || !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(label) = file_name(&path) else {
                continue;
            };
            let contents = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| read_error(&path, e))?;
            (stem.to_string(), vec![(label, contents)])
        };

        // Both layouts for one name is ambiguous, and guessing which wins would
        // be a variable silently taking a value nobody can find in the tree.
        if files.insert(name.clone(), parts).is_some() {
            return Err(ConnectorError {
                message: format!(
                    "'{}' is defined both as a file and as a directory",
                    dir.join(&name).display()
                ),
                stderr: String::new(),
                exit_code: None,
            });
        }
    }

    Ok(files)
}

// Every *.yaml / *.yml directly inside one group_vars/<name>/ directory,
// sorted by file name so the merge order is the one Ansible uses and does not
// depend on however the filesystem happened to return them. Nested
// directories are not descended into — Ansible does not either.
async fn read_vars_subdir(dir: &Path) -> Result<Vec<(String, String)>, ConnectorError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| read_error(dir, e))?;
    let mut paths = Vec::new();

    while let Some(entry) = entries.next_entry().await.map_err(|e| read_error(dir, e))? {
        let path = entry.path();
        if is_yaml(&path) && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut parts = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(base) = file_name(&path) else {
            continue;
        };
        let label = match file_name(dir) {
            Some(parent) => format!("{}/{}", parent, base),
            None => base,
        };
        let contents = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| read_error(&path, e))?;
        parts.push((label, contents));
    }
    Ok(parts)
}

fn is_yaml(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext == "yaml" || ext == "yml")
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
}

fn read_error(path: &Path, e: std::io::Error) -> ConnectorError {
    ConnectorError {
        message: format!("cannot read '{}': {}", path.display(), e),
        stderr: String::new(),
        exit_code: None,
    }
}
