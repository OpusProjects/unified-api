use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::config::{
    AppConfig, CONFIG_FILES, ConfigErrors, REQUIRED_CONFIG_FILE, load_config_detailed,
};
use crate::ports::config_store::{ConfigChange, ConfigFileStat, ConfigStorePort};

// CONFIG_DIR, as a store the running process can write back to.
//
// Two things here are worth more than the code that does them.
//
// First, a change is VALIDATED IN A STAGING DIRECTORY, never in place. The
// loader's only entry point is "load this directory", and the question being
// asked is about a directory that does not exist yet — the current one with
// three files replaced. Staging a copy is what lets the answer arrive before
// anything on disk has moved, which is the whole difference between "your
// change was rejected" and "your change was rejected and the instance is now
// running on it".
//
// Second, the commit writes every file to a temp name and only then renames
// them into place. rename(2) within a filesystem is atomic, so no reader ever
// sees a half-written file. Renames of SEVERAL files are not atomic as a
// group — a crash between two of them leaves some new and some old — but each
// file is whole, the set was validated together moments earlier, and the
// alternative (swapping a whole directory by rename) would break every
// deployment that bind-mounts the directory itself, which is all of them.
pub struct FsConfigStore {
    dir: PathBuf,
}

// Staging directories are named per process and per attempt so two concurrent
// writes cannot land in the same one.
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FsConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // The contents every known file WOULD have with `change` applied: what is
    // on disk, plus the writes, minus the deletions (and minus everything the
    // payload does not name, when pruning).
    fn effective(&self, change: &ConfigChange) -> Result<BTreeMap<String, String>, String> {
        let mut files = BTreeMap::new();

        for name in CONFIG_FILES {
            if change.prune && !change.write.contains_key(name) {
                continue;
            }
            if change.delete.contains(name) {
                continue;
            }
            if let Some(contents) = change.write.get(name) {
                files.insert(name.to_string(), contents.clone());
                continue;
            }
            match std::fs::read_to_string(self.dir.join(name)) {
                Ok(contents) => {
                    files.insert(name.to_string(), contents);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("read '{}': {}", name, e)),
            }
        }

        Ok(files)
    }

    fn stat_of(&self, name: &str, contents: &str) -> ConfigFileStat {
        let modified = std::fs::metadata(self.dir.join(name))
            .ok()
            .and_then(|m| m.modified().ok());
        ConfigFileStat {
            name: name.to_string(),
            size: contents.len() as u64,
            sha256: sha256_hex(contents),
            modified,
        }
    }
}

// Hex sha256 of a file's contents — the value the HTTP layer serves as an
// ETag. Content-addressed rather than mtime-based on purpose: a pipeline that
// rewrites a file with identical bytes has changed nothing, and should not be
// told it has.
pub fn sha256_hex(contents: &str) -> String {
    let digest = Sha256::digest(contents.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

// A directory-wide ETag: one hash over every file's name and hash, in load
// order. This is what a pipeline sends back as If-Match when it pushes the
// whole directory, so a push that would overwrite someone else's change is
// refused instead of silently winning.
pub fn directory_etag(stats: &[ConfigFileStat]) -> String {
    let mut hasher = Sha256::new();
    for stat in stats {
        hasher.update(stat.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(stat.sha256.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

// Removes the staging directory when it goes out of scope, however it goes
// out of scope — a rejected configuration is the common case here, and it
// must not leave a directory behind on every failed attempt.
struct Staging {
    path: PathBuf,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl ConfigStorePort for FsConfigStore {
    fn stat_all(&self) -> Result<Vec<ConfigFileStat>, String> {
        let mut stats = Vec::new();
        for name in CONFIG_FILES {
            match std::fs::read_to_string(self.dir.join(name)) {
                Ok(contents) => stats.push(self.stat_of(name, &contents)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("read '{}': {}", name, e)),
            }
        }
        Ok(stats)
    }

    fn read(&self, name: &str) -> Result<Option<(ConfigFileStat, String)>, String> {
        match std::fs::read_to_string(self.dir.join(name)) {
            Ok(contents) => Ok(Some((self.stat_of(name, &contents), contents))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read '{}': {}", name, e)),
        }
    }

    fn load(&self, change: &ConfigChange) -> Result<AppConfig, ConfigErrors> {
        let files = self
            .effective(change)
            .map_err(|e| ConfigErrors::new(vec![e]))?;

        // Nothing to stage: the caller is validating what is already there.
        if change.is_empty() {
            return load_config_detailed(&self.dir);
        }

        if !files.contains_key(REQUIRED_CONFIG_FILE) {
            return Err(ConfigErrors::new(vec![format!(
                "{} would be missing — it is the one file a configuration cannot start without",
                REQUIRED_CONFIG_FILE
            )]));
        }

        let stage = Staging {
            path: self.dir.join(format!(
                ".config-api-stage-{}-{}",
                std::process::id(),
                STAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
            )),
        };
        std::fs::create_dir_all(&stage.path).map_err(|e| {
            ConfigErrors::new(vec![format!(
                "create staging directory '{}': {}",
                stage.path.display(),
                e
            )])
        })?;
        for (name, contents) in &files {
            std::fs::write(stage.path.join(name), contents)
                .map_err(|e| ConfigErrors::new(vec![format!("stage '{}': {}", name, e)]))?;
        }

        load_config_detailed(&stage.path)
    }

    fn commit(&self, change: &ConfigChange) -> Result<(), String> {
        let files = self.effective(change)?;

        // Write every file first, rename every file second. The window in
        // which the directory holds a mix of old and new is the duration of
        // the renames rather than the duration of the writes.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (name, contents) in &files {
            let target = self.dir.join(name);
            let tmp = self.dir.join(format!(".{}.tmp", name));
            std::fs::write(&tmp, contents)
                .map_err(|e| format!("write '{}': {}", tmp.display(), e))?;
            staged.push((tmp, target));
        }
        for (tmp, target) in &staged {
            std::fs::rename(tmp, target).map_err(|e| {
                format!(
                    "rename '{}' to '{}': {}",
                    tmp.display(),
                    target.display(),
                    e
                )
            })?;
        }

        // Whatever `files` does not contain is gone — either explicitly
        // deleted or pruned away by a whole-directory push.
        for name in CONFIG_FILES {
            if files.contains_key(name) {
                continue;
            }
            match std::fs::remove_file(self.dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove '{}': {}", name, e)),
            }
        }

        Ok(())
    }

    fn location(&self) -> String {
        self.dir.display().to_string()
    }
}
