use std::path::Path;

// Where a script actually lives: inside the project checkout when the file is
// there, otherwise wherever the configured path points.
//
// Resolved at EXECUTION time, not at boot. Boot-time resolution (the original
// design) froze whatever the disk looked like during startup: a script that
// arrived with the first successful clone after boot kept its unresolved path
// until a restart, and serving could not begin until every clone had been
// awaited. Per-execution resolution is one stat() next to spawning a whole
// process, and it makes the checkout state at THIS run the one that decides.
//
// The rules stay deliberately conservative, exactly as boot resolution was:
// - absolute paths are kept as-is
// - the checkout wins only when the file actually exists inside it
// - otherwise the configured path stays (it may be baked into the image or
//   mounted) and execution proceeds exactly as before projects existed
//
// Callers skip SSH sources — their script_path is a REMOTE command, not a file
// on this machine.
pub fn resolve_script_path(
    projects_dir: &Path,
    owner_id: &str,
    project_id: &str,
    script_path: &str,
) -> String {
    if Path::new(script_path).is_absolute() {
        return script_path.to_string();
    }

    let candidate = projects_dir.join(project_id).join(script_path);
    if candidate.is_file() {
        tracing::debug!(id = %owner_id, path = %candidate.display(), "Script resolved in project checkout");
        return candidate.to_string_lossy().into_owned();
    }

    if projects_dir.join(project_id).is_dir() {
        // The checkout exists but the script is not in it — likely a typo in
        // config. Keep the original path (it may still resolve against the
        // working directory) but say something.
        tracing::warn!(
            id = %owner_id,
            project = %project_id,
            script = %script_path,
            "Script not found in project checkout, using the configured path"
        );
    }
    script_path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_script_inside_the_checkout_is_resolved_into_it() {
        let projects_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(projects_dir.path().join("prj-test")).unwrap();
        fs::write(projects_dir.path().join("prj-test/fetch.py"), "#!/bin/sh\n").unwrap();

        let resolved = resolve_script_path(projects_dir.path(), "src-test", "prj-test", "fetch.py");

        assert!(resolved.ends_with("prj-test/fetch.py"));
        assert!(resolved.starts_with(projects_dir.path().to_str().unwrap()));
    }

    #[test]
    fn a_missing_checkout_keeps_the_configured_path() {
        // No checkout at all (clone failed / has not landed yet): the path
        // must not change, so scripts baked into the image keep working
        let projects_dir = tempfile::tempdir().unwrap();

        let resolved = resolve_script_path(
            projects_dir.path(),
            "src-test",
            "prj-test",
            "local/fetch.py",
        );

        assert_eq!(resolved, "local/fetch.py");
    }

    #[test]
    fn a_script_missing_from_an_existing_checkout_keeps_the_configured_path() {
        let projects_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(projects_dir.path().join("prj-test")).unwrap();

        let resolved = resolve_script_path(projects_dir.path(), "src-test", "prj-test", "typo.py");

        assert_eq!(resolved, "typo.py");
    }

    #[test]
    fn an_absolute_path_is_never_touched() {
        let projects_dir = tempfile::tempdir().unwrap();

        let resolved =
            resolve_script_path(projects_dir.path(), "src-test", "prj-test", "/opt/fetch.py");

        assert_eq!(resolved, "/opt/fetch.py");
    }

    // The reason resolution moved out of boot: a checkout that appears AFTER
    // startup (slow clone, first pipeline push) takes effect on the next run,
    // not the next restart.
    #[test]
    fn a_checkout_appearing_later_is_picked_up() {
        let projects_dir = tempfile::tempdir().unwrap();

        let before = resolve_script_path(projects_dir.path(), "src-test", "prj-test", "fetch.py");
        assert_eq!(before, "fetch.py");

        fs::create_dir_all(projects_dir.path().join("prj-test")).unwrap();
        fs::write(projects_dir.path().join("prj-test/fetch.py"), "#!/bin/sh\n").unwrap();

        let after = resolve_script_path(projects_dir.path(), "src-test", "prj-test", "fetch.py");
        assert!(after.ends_with("prj-test/fetch.py"));
    }
}
