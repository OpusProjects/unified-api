// The --check-config flag through the real binary: the exit code is the
// contract a CI pipeline scripts against, so it is what gets tested — not the
// validation logic itself, which has its own tests in config.rs.

use std::process::Command;

fn run_check(config_dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .arg("--check-config")
        .env("CONFIG_DIR", config_dir)
        .output()
        .expect("the binary runs")
}

#[test]
fn a_valid_config_directory_exits_zero_and_says_ok() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
    )
    .unwrap();

    let output = run_check(dir.path());

    assert!(output.status.success(), "expected exit 0: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("configuration OK"), "stdout: {}", stdout);
}

#[test]
fn a_typoed_key_exits_one_and_names_the_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        // "porT" is the strict-parsing trap --check-config exists to catch
        "server:\n  host: \"127.0.0.1\"\n  porT: 9090\n",
    )
    .unwrap();

    let output = run_check(dir.path());

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("configuration INVALID") && stderr.contains("porT"),
        "stderr: {}",
        stderr
    );
}

#[test]
fn a_broken_cross_reference_fails_like_startup_would() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
    )
    .unwrap();
    // A source naming a project that does not exist — valid YAML, invalid config
    std::fs::write(
        dir.path().join("sources.yaml"),
        "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-a\n    project: \"prj-missing\"\n    path: \"x.py\"\n",
    )
    .unwrap();

    let output = run_check(dir.path());

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("prj-missing"), "stderr: {}", stderr);
}

// A directory named after the flag is the one checked, over CONFIG_DIR: CI
// that passes a path must never be told OK about another directory.
#[test]
fn a_directory_after_the_flag_is_the_one_checked() {
    let good = tempfile::tempdir().unwrap();
    std::fs::write(
        good.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
    )
    .unwrap();
    let bad = tempfile::tempdir().unwrap();
    std::fs::write(
        bad.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  porT: 9090\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .arg("--check-config")
        .arg(bad.path())
        .env("CONFIG_DIR", good.path())
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&bad.path().display().to_string()) && stderr.contains("porT"),
        "stderr: {stderr}"
    );
}

// The configuration the image ships with (docker/Dockerfile copies config/)
// must start as it is, in this version's own shape.
#[test]
fn the_bundled_config_is_valid_as_it_is() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
    let output = run_check(&dir);
    assert!(output.status.success(), "{output:?}");
}

// A directory an older version wrote is not converted: it is refused, naming
// what to write instead.
#[test]
fn a_0_35_ssh_directory_is_refused_saying_what_to_write() {
    let dir = tempfile::tempdir().unwrap();
    let write = |path: &str, body: &str| {
        let file = dir.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    };
    write(
        "config.yaml",
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
    );
    write(
        "connectors/ssh.yaml",
        "connectors:\n  - name: ssh-gather-facts\n    type: ssh\n    path: gather_facts\n",
    );

    let output = run_check(dir.path());

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("'ssh-gather-facts' is in the 0.35 form"),
        "{stderr}"
    );
}
