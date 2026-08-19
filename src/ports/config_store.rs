use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use crate::config::{AppConfig, ConfigErrors};

// The configuration directory as something the application can read, propose
// changes to, and commit — rather than as a directory the process happens to
// have read once at startup.
//
// A port, so the use case is written against "a place configuration lives"
// and the filesystem details (staging, atomic renames, which temp file) stay
// in the adapter. The types are deliberately about WHOLE FILES: this mirrors
// the deployment unit a configuration-as-code pipeline already works in — it
// renders `sources.yaml`, not a patch to a source — and it keeps YAML
// comments and key order intact, which any parse-and-reserialize scheme
// would quietly destroy.

// One file's identity without its contents: enough to list a directory, and
// enough to answer "did this change" without shipping every byte.
#[derive(Debug, Clone)]
pub struct ConfigFileStat {
    pub name: String,
    pub size: u64,
    // Hex sha256 of the contents. Also the file's ETag — see the HTTP adapter.
    pub sha256: String,
    pub modified: Option<SystemTime>,
}

// A proposed change to the directory, as one unit.
//
// Everything is applied together or not at all: a pipeline that renders five
// files has no interest in a state where three landed, and a cross-file
// reference (a source naming a project) can only be validated against the
// whole set anyway.
#[derive(Debug, Clone, Default)]
pub struct ConfigChange {
    // File name -> contents. Files not named here keep whatever is on disk,
    // unless `prune` says otherwise.
    pub write: BTreeMap<String, String>,
    pub delete: BTreeSet<String>,
    // Delete every known file that `write` does not name: the directory ends
    // up being exactly the payload. This is what makes a push idempotent —
    // the same semantics as the configuration image it replaces, where what
    // is not in the image is not in /config.
    pub prune: bool,
}

impl ConfigChange {
    pub fn writing(write: BTreeMap<String, String>) -> Self {
        Self {
            write,
            ..Self::default()
        }
    }

    pub fn deleting(name: &str) -> Self {
        Self {
            delete: [name.to_string()].into_iter().collect(),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.write.is_empty() && self.delete.is_empty() && !self.prune
    }
}

pub trait ConfigStorePort: Send + Sync {
    // Every known configuration file that is present, in load order.
    fn stat_all(&self) -> Result<Vec<ConfigFileStat>, String>;

    // One file with its contents. Ok(None) = the file is not there, which for
    // every file but config.yaml is a legitimate configuration.
    fn read(&self, name: &str) -> Result<Option<(ConfigFileStat, String)>, String>;

    // Load the directory as it WOULD be with `change` applied, without
    // touching it. An empty change validates what is on disk right now —
    // exactly what `--check-config` does, from inside the running process.
    fn load(&self, change: &ConfigChange) -> Result<AppConfig, ConfigErrors>;

    // Apply the change. Callers validate first (`load`) — commit does not,
    // because "write this even though it does not load" is a thing an
    // operator can legitimately want and a thing the API refuses on its own.
    fn commit(&self, change: &ConfigChange) -> Result<(), String>;

    // Where this store keeps the files, for the operator reading the response.
    fn location(&self) -> String;
}
