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
        "src-a:\n  name: \"A\"\n  project_id: \"prj-missing\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
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
