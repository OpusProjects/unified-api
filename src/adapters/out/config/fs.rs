use std::path::PathBuf;

use crate::config::{AppConfig, load_config_detailed};

// CONFIG_DIR, as a store the running process can write back to: web-rs's
// FsConfigStore (the `web` repository), which validates a change in a staged
// copy and commits it file by file with atomic renames, loading with this
// service's own loader — the one startup and --check-config use.
pub type FsConfigStore = web_rs::config::FsConfigStore<AppConfig>;

pub use web_rs::config::{directory_etag, sha256_hex};

// The store over `dir`, loading with this service's loader.
pub fn fs_store(dir: impl Into<PathBuf>) -> FsConfigStore {
    FsConfigStore::new(dir, load_config_detailed)
}
