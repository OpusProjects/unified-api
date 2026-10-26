// Upgrading a 0.34 deployment in place: the binary meets a configuration
// directory still in the eight-file layout and converts it before loading.
//
// The fixture (tests/fixtures/legacy-0.34) has the shapes of a real
// aggregator's configuration — shared inventory scripts, SSH facts chained off
// an inventory, a remote script in script_path, federation, a static
// inventory, a view, a constructed endpoint, an enricher, restricted keys and
// LDAP sign-in. What is tested is what an operator relies on: the pod starts,
// serves the same ids, and keeps the old files where they can be found.

use std::path::{Path, PathBuf};
use std::process::Command;
use unified_api::config::load_config;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-0.34")
}

// A writable copy of the fixture: the migration works in place.
fn copy_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for entry in std::fs::read_dir(fixture()).expect("fixture") {
        let entry = entry.expect("entry");
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).expect("copy");
    }
    dir
}

fn files_under(dir: &Path) -> Vec<String> {
    unified_api::config::config_files(dir).expect("walk")
}

#[test]
fn check_config_validates_the_conversion_without_writing() {
    let dir = copy_fixture();
    let before = files_under(dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .arg("--check-config")
        .env("CONFIG_DIR", dir.path())
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("in the 0.34 layout"), "{stdout}");
    assert!(
        stdout.contains("connector 'd42' extracted from: src-d42-cdi, src-d42-iii, src-d42-pq"),
        "{stdout}"
    );
    assert_eq!(files_under(dir.path()), before, "a check writes nothing");
}

#[test]
fn migrate_config_converts_every_item_and_keeps_the_old_files() {
    let dir = copy_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .args(["--migrate-config", dir.path().to_str().expect("utf-8")])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{output:?}");

    // The old files moved to one hidden backup folder, comments and all.
    let backups: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".legacy-config-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    let old_sources = std::fs::read_to_string(backups[0].join("sources.yaml")).unwrap();
    assert!(old_sources.contains("# Three tenants of the same CMDB"));
    assert!(!dir.path().join("sources.yaml").exists());

    // One file per item, by section.
    let files = files_under(dir.path());
    for expected in [
        "config.yaml",
        "connectors/d42.yaml",
        "connectors/ssh-gather-facts.yaml",
        "connectors/ssh-script.yaml",
        "connectors/vmware.yaml",
        "connectors/vmware-2.yaml",
        "sources/src-d42-pq.yaml",
        "sources/src-edge-pq-dc4.yaml",
        "views/vw-pq-facts.yaml",
        "endpoints/ep-ansible-full.yaml",
        "enrichers/enr-vmware-onto-d42.yaml",
        "projects/prj-ansible-inventory.yaml",
        "credentials/cred-ldap.yaml",
        "api_keys/key-dashboard.yaml",
    ] {
        assert!(
            files.contains(&expected.to_string()),
            "{expected} in {files:?}"
        );
    }

    let cfg = load_config(dir.path().to_str().unwrap()).expect("the new layout loads");
    assert_eq!(cfg.sources.len(), 12);
    assert_eq!(cfg.connectors.len(), 6);

    // Same ids, same labels: a consumer sees no difference.
    let pq = &cfg.sources["src-d42-pq"];
    assert_eq!(pq.name, "Device42 PQ Inventory");
    assert_eq!(pq.connector, "d42");
    assert_eq!(pq.script_path, "dynamic/d42/inventory.py");
    assert_eq!(pq.host_args, vec!["--host-with-groups", "{target}"]);
    assert_eq!(pq.credential_ids, vec!["cred-d42-pq"]);
    assert_eq!(pq.config["D42_CONFIG_DIR"], "yaml/pq");

    // The connector the three tenants share holds what they had in common.
    let d42 = &cfg.connectors["d42"];
    assert_eq!(d42.project.as_deref(), Some("prj-ansible-inventory"));
    assert_eq!(d42.path, "dynamic/d42/inventory.py");
    assert_eq!(d42.script_args, vec!["--list"]);

    // ssh: the remote script survived byte for byte, the unused project went.
    let os = &cfg.sources["src-ssh-cdi-osinfo"];
    assert_eq!(os.connector, "ssh-script");
    assert!(os.script_path.starts_with("ID=; VERSION_ID=\n"));
    assert!(os.script_path.ends_with("\"$ID\" \"$VERSION_ID\""));
    assert_eq!(os.project_id, None);
    assert_eq!(os.timeout_seconds, 600);

    // Remote: the remote source id moved from script_path into config.
    let edge = &cfg.sources["src-edge-pq-dc4"];
    assert_eq!(edge.connector, "remote");
    assert_eq!(edge.config["source"], "src-ssh-facts");
    assert_eq!(edge.script_path, "src-ssh-facts");
    assert_eq!(edge.project_id, None);

    // Static inventory: project + path on the source.
    let vars = &cfg.sources["src-pcd-inventory-pq_vars"];
    assert_eq!(vars.connector, "static_inventory");
    assert_eq!(vars.script_path, "static/pq/inventory.yaml");
    assert_eq!(vars.project_id.as_deref(), Some("prj-ansible-inventory"));

    // A different timeout than its connector's stays on the source.
    assert_eq!(cfg.sources["src-vmware-fqdn"].timeout_seconds, 1800);
    assert_eq!(cfg.sources["src-vmware"].timeout_seconds, 300);

    // References lost their _id suffix and still point at the same items.
    assert_eq!(cfg.enrichers["enr-vmware-onto-d42"].target_id, "src-d42-pq");
    assert_eq!(
        cfg.projects["prj-ansible-inventory"]
            .credential_id
            .as_deref(),
        Some("cred-git")
    );
    assert_eq!(cfg.endpoints["ep-ansible-full"].source_ids.len(), 6);
    assert_eq!(
        cfg.api_keys["key-dashboard"].endpoints,
        vec!["ep-ansible-full"]
    );

    // Once is once.
    let again = Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .args(["--migrate-config", dir.path().to_str().unwrap()])
        .output()
        .expect("the binary runs");
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("nothing to do"));
}

// A port nothing listens on, for the startup test.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

// The real upgrade: a 0.34 directory on the volume, the new binary started on
// it. It must come up, serve the same sources, and leave the new layout behind.
#[tokio::test]
async fn a_0_34_directory_is_migrated_at_startup_and_served() {
    let dir = copy_fixture();
    let port = free_port();
    let config = std::fs::read_to_string(dir.path().join("config.yaml"))
        .unwrap()
        .replace("port: 9090", &format!("port: {port}"))
        .replace(
            "projects:\n  dir: \"projects\"",
            &format!(
                "projects:\n  dir: \"{}\"",
                dir.path().join("checkouts").display()
            ),
        );
    std::fs::write(dir.path().join("config.yaml"), config).unwrap();

    // Dummy values: the keys must resolve for the process to start, and
    // nothing here talks to a real system.
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .env("CONFIG_DIR", dir.path())
        .env("UNIFIED_API_KEY_AUTOMATION", "test-admin-key")
        .env("UNIFIED_API_KEY_DASHBOARD", "test-dashboard-key")
        .env("RUST_LOG", "warn")
        // The log goes to stdout (tracing's default)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the binary starts");

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut up = false;
    for _ in 0..100 {
        if client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !up {
        let _ = child.kill().await;
        let output = child.wait_with_output().await.expect("output");
        panic!(
            "never became healthy: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let sources: Vec<serde_json::Value> = client
        .get(format!("{base}/api/v1/sources"))
        .header("x-api-key", "test-admin-key")
        .send()
        .await
        .expect("sources")
        .json()
        .await
        .expect("json");
    let mut ids: Vec<&str> = sources
        .iter()
        .map(|s| s["source_id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert!(
        ids.contains(&"src-d42-pq") && ids.contains(&"vw-pq-facts"),
        "{ids:?}"
    );
    assert_eq!(ids.len(), 13, "twelve sources and one view: {ids:?}");

    let connectors: Vec<serde_json::Value> = client
        .get(format!("{base}/api/v1/connectors"))
        .header("x-api-key", "test-admin-key")
        .send()
        .await
        .expect("connectors")
        .json()
        .await
        .expect("json");
    let d42 = connectors
        .iter()
        .find(|c| c["connector_id"] == "d42")
        .expect("d42");
    assert_eq!(d42["used_by"].as_array().unwrap().len(), 3);

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("output");
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("0.34 layout and has been migrated"), "{log}");

    assert!(!dir.path().join("sources.yaml").exists());
    assert!(dir.path().join("connectors/d42.yaml").is_file());
}

// The other half of "never bricked": a 0.34 directory the conversion cannot
// load refuses to start AND is left exactly as it was, so the previous image
// can still be rolled back onto it.
#[test]
fn a_directory_that_would_not_convert_is_left_alone_and_startup_refuses() {
    let dir = copy_fixture();
    let sources = dir.path().join("sources.yaml");
    let broken = std::fs::read_to_string(&sources).unwrap().replace(
        "credential_ids: [\"cred-vcenter\"]",
        "credential_ids: [\"cred-nowhere\"]",
    );
    std::fs::write(&sources, &broken).unwrap();
    let before = files_under(dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_unified-api"))
        .env("CONFIG_DIR", dir.path())
        .env("RUST_LOG", "error")
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let log = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("could not be migrated"), "{log}");
    assert!(log.contains("cred-nowhere"), "{log}");
    assert_eq!(files_under(dir.path()), before);
    assert_eq!(std::fs::read_to_string(&sources).unwrap(), broken);
    let hidden: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(hidden.is_empty(), "no backup or staging left behind");
}
