// Integration tests for the static inventory connector: build a real
// inventory layout on disk (inventory.yaml + group_vars/ + host_vars/,
// the same shape as an inventories git repo) and read it through the adapter.
use std::collections::HashMap;
use std::path::Path;

use unified_api::adapters::out::connectors::static_inventory::StaticInventoryConnector;
use unified_api::domain::source::OutputFormat;
use unified_api::ports::connector::ConnectorPort;

async fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }
    tokio::fs::write(path, contents).await.unwrap();
}

async fn make_inventory_repo(dir: &Path) {
    write(
        &dir.join("inventory.yaml"),
        r#"
all:
  hosts:
    localhost:
      ansible_connection: local
  children:
    zookeeper:
      hosts:
        zk01.example.com: {}
        zk02.example.com: {}
    nas:
      hosts:
        nas01.example.com: {}
"#,
    )
    .await;
    write(
        &dir.join("group_vars/all.yaml"),
        "timezone: Europe/Madrid\nuseransible: laughingman_ansible\n",
    )
    .await;
    write(&dir.join("group_vars/zookeeper.yaml"), "zk_port: 2181\n").await;
    write(
        &dir.join("host_vars/nas01.example.com.yaml"),
        "nas_cert_uuid: \"59343d18\"\n",
    )
    .await;
}

#[tokio::test]
async fn reads_a_full_inventory_layout_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    make_inventory_repo(dir.path()).await;

    let connector = StaticInventoryConnector::new();
    let result = connector
        .execute(
            dir.path().join("inventory.yaml").to_str().unwrap(),
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;

    let dataset = result.expect("static inventory must parse");

    assert_eq!(dataset.hostvars.len(), 4);
    // group_vars/all reaches every host
    assert_eq!(dataset.hostvars["localhost"]["timezone"], "Europe/Madrid");
    // group_vars/<group> reaches its members only
    assert_eq!(dataset.hostvars["zk01.example.com"]["zk_port"], 2181);
    assert!(!dataset.hostvars["nas01.example.com"].contains_key("zk_port"));
    // host_vars file
    assert_eq!(
        dataset.hostvars["nas01.example.com"]["nas_cert_uuid"],
        "59343d18"
    );
    // groups: all is implicit, the rest are real
    assert_eq!(dataset.groups.len(), 2);
    assert_eq!(
        dataset.groups["zookeeper"].hosts,
        vec!["zk01.example.com", "zk02.example.com"]
    );
}

#[tokio::test]
async fn inventory_without_vars_dirs_still_works() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("inventory.yaml"),
        "all:\n  hosts:\n    solo.example.com: {}\n",
    )
    .await;

    let connector = StaticInventoryConnector::new();
    let dataset = connector
        .execute(
            dir.path().join("inventory.yaml").to_str().unwrap(),
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .expect("no group_vars/host_vars is a valid layout");

    assert_eq!(dataset.hostvars.len(), 1);
}

#[tokio::test]
async fn missing_inventory_file_is_a_clear_error() {
    let connector = StaticInventoryConnector::new();
    let err = connector
        .execute(
            "/nonexistent/inventory.yaml",
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .expect_err("missing file must fail");
    assert!(err.message.contains("cannot read inventory file"));
}

#[tokio::test]
async fn vaulted_host_vars_fail_the_sync_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    make_inventory_repo(dir.path()).await;
    write(
        &dir.path().join("host_vars/zk01.example.com.yaml"),
        "$ANSIBLE_VAULT;1.1;AES256\n61383061...",
    )
    .await;

    let connector = StaticInventoryConnector::new();
    let err = connector
        .execute(
            dir.path().join("inventory.yaml").to_str().unwrap(),
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .expect_err("vaulted content must fail");
    assert!(err.message.contains("ansible-vault"));
    assert!(err.message.contains("zk01.example.com"));
}
