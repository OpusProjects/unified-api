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
        //
        // `files` is the whole directory, not just the payload — that is what
        // makes the change transactional. But a file whose bytes are already
        // what we would write is SKIPPED, because writing it would reset an
        // mtime that nothing changed, and mtime is exactly what
        // GET /api/v1/config reports as `modified`. A single-file PUT used to
        // stamp all eight files with the time of the request, so "when did
        // credentials.yaml last change" answered "just now" forever.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (name, contents) in &files {
            let target = self.dir.join(name);
            if std::fs::read_to_string(&target).is_ok_and(|current| current == *contents) {
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dir_with_minimal_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .expect("write config.yaml");
        dir
    }

    fn writing(pairs: &[(&str, &str)]) -> ConfigChange {
        let files: BTreeMap<String, String> = pairs
            .iter()
            .map(|(name, contents)| (name.to_string(), contents.to_string()))
            .collect();
        ConfigChange::writing(files)
    }

    #[test]
    fn a_rejected_change_leaves_the_directory_exactly_as_it_was() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());
        let before = std::fs::read_to_string(dir.path().join("config.yaml")).expect("read");

        // A source pointing at a project nobody declared: valid YAML, invalid
        // configuration — the case that must never reach the disk.
        let change = writing(&[(
            "sources.yaml",
            "src-a:\n  name: \"A\"\n  project_id: \"prj-ghost\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
        )]);

        let errors = store.load(&change).err().expect("must be rejected");
        assert!(
            errors.errors.iter().any(|e| e.contains("prj-ghost")),
            "errors: {:?}",
            errors.errors
        );
        assert!(!dir.path().join("sources.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.yaml")).expect("read"),
            before
        );
    }

    #[test]
    fn validating_leaves_no_staging_directory_behind() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());

        let _ = store.load(&writing(&[("sources.yaml", "not: [valid\n")]));
        let _ = store.load(&writing(&[("enrichers.yaml", "")]));

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".config-api-stage"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {:?}", leftovers);
    }

    #[test]
    fn a_commit_leaves_no_temporary_files_behind() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());

        let change = writing(&[(
            "credentials.yaml",
            "cred-a:\n  name: \"A\"\n  type: \"token\"\n  env_prefix: \"A\"\n",
        )]);
        store.load(&change).expect("valid");
        store.commit(&change).expect("commits");

        assert!(dir.path().join("credentials.yaml").exists());
        let temporaries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temporaries.is_empty(), "left behind: {:?}", temporaries);
    }

    // A write of one file must not restamp the other seven. `modified` in the
    // inventory is what an operator reads to answer "when did this last
    // change", and a commit that rewrites the whole directory makes every
    // file answer "just now" whether or not anything happened to it.
    #[test]
    fn a_commit_leaves_the_files_it_does_not_change_alone() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());

        let untouched = || {
            std::fs::metadata(dir.path().join("config.yaml"))
                .and_then(|m| m.modified())
                .expect("mtime")
        };
        let before = untouched();

        // Long enough that a rewrite would land on a different mtime, so this
        // test fails if the skip is ever removed.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let change = writing(&[(
            "credentials.yaml",
            "cred-a:\n  name: \"A\"\n  type: \"token\"\n  env_prefix: \"A\"\n",
        )]);
        store.commit(&change).expect("commits");

        assert_eq!(
            before,
            untouched(),
            "config.yaml was not part of the change and must not have been rewritten"
        );
        assert!(dir.path().join("credentials.yaml").exists());

        // And re-committing identical bytes is a no-op on disk too.
        let written = std::fs::metadata(dir.path().join("credentials.yaml"))
            .and_then(|m| m.modified())
            .expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(50));
        store.commit(&change).expect("commits");
        assert_eq!(
            written,
            std::fs::metadata(dir.path().join("credentials.yaml"))
                .and_then(|m| m.modified())
                .expect("mtime"),
            "re-pushing the same bytes has changed nothing and must not say otherwise"
        );
    }

    #[test]
    fn pruning_removes_the_files_the_push_left_out() {
        let dir = dir_with_minimal_config();
        std::fs::write(
            dir.path().join("credentials.yaml"),
            "cred-a:\n  name: \"A\"\n  type: \"token\"\n  env_prefix: \"A\"\n",
        )
        .expect("write");
        let store = FsConfigStore::new(dir.path());

        // The whole directory, pushed as config.yaml alone: credentials.yaml
        // is not in the payload, so it must not be in the directory either.
        let change = ConfigChange {
            write: [(
                "config.yaml".to_string(),
                "server:\n  host: \"127.0.0.1\"\n  port: 9090\n".to_string(),
            )]
            .into_iter()
            .collect(),
            prune: true,
            ..ConfigChange::default()
        };
        store.load(&change).expect("valid");
        store.commit(&change).expect("commits");

        assert!(dir.path().join("config.yaml").exists());
        assert!(!dir.path().join("credentials.yaml").exists());
    }

    #[test]
    fn pruning_away_config_yaml_is_refused_before_anything_is_written() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());

        let change = ConfigChange {
            write: [("enrichers.yaml".to_string(), String::new())]
                .into_iter()
                .collect(),
            prune: true,
            ..ConfigChange::default()
        };

        let errors = store.load(&change).err().expect("must be rejected");
        assert!(
            errors.errors[0].contains("config.yaml"),
            "errors: {:?}",
            errors.errors
        );
        assert!(dir.path().join("config.yaml").exists());
    }

    #[test]
    fn deleting_a_file_the_rest_depends_on_is_rejected() {
        let dir = dir_with_minimal_config();
        std::fs::write(
            dir.path().join("projects.yaml"),
            "prj-a:\n  name: \"A\"\n  git_url: \"https://example.invalid/a.git\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("sources.yaml"),
            "src-a:\n  name: \"A\"\n  project_id: \"prj-a\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
        )
        .expect("write");
        let store = FsConfigStore::new(dir.path());

        let errors = store
            .load(&ConfigChange::deleting("projects.yaml"))
            .err()
            .expect("must be rejected");

        assert!(
            errors.errors.iter().any(|e| e.contains("prj-a")),
            "errors: {:?}",
            errors.errors
        );
        assert!(dir.path().join("projects.yaml").exists());
    }

    #[test]
    fn an_unparseable_file_is_reported_with_its_name() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());

        let errors = store
            .load(&writing(&[(
                "sources.yaml",
                "src-a:\n  ttl_seconds: \"not a number\"\n",
            )]))
            .err()
            .expect("must be rejected");

        assert!(
            errors.errors[0].starts_with("sources.yaml:"),
            "a parse error has to say which of eight files it is about: {:?}",
            errors.errors
        );
    }

    #[test]
    fn the_directory_etag_follows_the_contents() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());
        let before = directory_etag(&store.stat_all().expect("stat"));

        let change = writing(&[(
            "config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9091\n",
        )]);
        store.commit(&change).expect("commits");
        let after = directory_etag(&store.stat_all().expect("stat"));

        assert_ne!(before, after, "a changed file must change the ETag");

        // Rewriting identical bytes changes nothing, so it must not change the
        // ETag either — a pipeline that re-pushes the same files has not
        // "modified" anything and should not be told it has.
        store.commit(&change).expect("commits");
        assert_eq!(after, directory_etag(&store.stat_all().expect("stat")));
    }

    #[test]
    fn an_empty_change_validates_what_is_already_on_disk() {
        let dir = dir_with_minimal_config();
        let store = FsConfigStore::new(dir.path());
        assert!(store.load(&ConfigChange::default()).is_ok());

        std::fs::write(dir.path().join("config.yaml"), "server:\n  porT: 9090\n").expect("write");
        let errors = store
            .load(&ConfigChange::default())
            .err()
            .expect("a typo'd key must fail");
        assert!(
            errors.errors[0].contains("porT"),
            "errors: {:?}",
            errors.errors
        );
    }
}
