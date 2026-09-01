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
        "timezone: UTC\nuseransible: laughingman_ansible\n",
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

    let dataset = result.expect("static inventory must parse").dataset;

    assert_eq!(dataset.hostvars.len(), 4);
    // group_vars/all lands on `all`, held once instead of per host
    let all = dataset.groups["all"]
        .vars
        .as_ref()
        .expect("all carries vars");
    assert_eq!(all["timezone"], "UTC");
    assert_eq!(all["useransible"], "laughingman_ansible");
    assert!(!dataset.hostvars["localhost"].contains_key("timezone"));
    // group_vars/<group> lands on that group
    assert_eq!(
        dataset.groups["zookeeper"].vars.as_ref().unwrap()["zk_port"],
        2181
    );
    assert!(dataset.groups["nas"].vars.is_none());
    // host_vars file is the host's own, so it stays on the host
    assert_eq!(
        dataset.hostvars["nas01.example.com"]["nas_cert_uuid"],
        "59343d18"
    );
    // groups: `all` is emitted too, so three rather than two
    assert_eq!(dataset.groups.len(), 3);
    assert_eq!(dataset.groups["all"].hosts, vec!["localhost"]);
    assert_eq!(dataset.groups["all"].children, vec!["nas", "zookeeper"]);
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
        .expect("no group_vars/host_vars is a valid layout")
        .dataset;

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

// =========================================================================
// Tests: the connector's failure shapes (missing file, unparseable file)
// =========================================================================
#[tokio::test]
async fn a_missing_inventory_file_is_a_named_failure() {
    let connector = StaticInventoryConnector::new();
    let error = connector
        .execute(
            "/does/not/exist/inventory.yaml",
            &[],
            OutputFormat::Native,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        )
        .await
        .expect_err("missing file must fail");
    assert!(
        error.message.contains("/does/not/exist/inventory.yaml"),
        "error was: {}",
        error.message
    );
}

#[tokio::test]
async fn an_unparseable_inventory_is_a_named_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inventory.yaml");
    std::fs::write(&path, "this: [is: not\nvalid yaml inventory").unwrap();

    let connector = StaticInventoryConnector::new();
    let error = connector
        .execute(
            path.to_str().unwrap(),
            &[],
            OutputFormat::Native,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        )
        .await
        .expect_err("garbage YAML must fail");
    assert!(
        error.message.contains("inventory.yaml"),
        "error was: {}",
        error.message
    );
}

// group_vars/ that exists but cannot be parsed must fail rather than serve an
// inventory silently missing its vars
#[tokio::test]
async fn unparseable_group_vars_fail_rather_than_vanish() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("inventory.yaml"),
        "all:\n  hosts:\n    a.example:\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("group_vars")).unwrap();
    std::fs::write(dir.path().join("group_vars/all.yaml"), "key: [unclosed").unwrap();

    let connector = StaticInventoryConnector::new();
    let result = connector
        .execute(
            dir.path().join("inventory.yaml").to_str().unwrap(),
            &[],
            OutputFormat::Native,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        )
        .await;
    assert!(result.is_err(), "unparseable group_vars must not be silent");
}

// Ansible accepts group_vars/web.yaml or group_vars/web/ holding several
// files. Only the flat form was read, so an inventory written the other way
// came back with every host and no variables at all — which is how a real
// estate of 119 group_vars directories produced 1097 hosts and zero vars.
#[tokio::test]
async fn group_vars_and_host_vars_may_be_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("inventory.yaml"),
        r#"
all:
  hosts:
    web01.example.com: {}
  children:
    web:
      hosts:
        web01.example.com: {}
"#,
    )
    .await;
    // one concern per file, the reason the directory form exists
    write(
        &dir.path().join("group_vars/all/ntp.yaml"),
        "ntp: pool.ntp\n",
    )
    .await;
    write(
        &dir.path().join("group_vars/all/ssh.yaml"),
        "useransible: pq_ansible\n",
    )
    .await;
    write(
        &dir.path().join("group_vars/web/http.yml"),
        "http_port: 8080\n",
    )
    .await;
    write(
        &dir.path().join("host_vars/web01.example.com/disk.yaml"),
        "disk: ssd\n",
    )
    .await;
    // not YAML, and a nested directory: neither is read
    write(&dir.path().join("group_vars/all/README.md"), "notes\n").await;
    write(
        &dir.path().join("group_vars/all/nested/deep.yaml"),
        "ignored: true\n",
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
        .expect("a directory layout must parse")
        .dataset;

    // every file in group_vars/all/ merged, not just one of them -- on the
    // group, which is where a group's vars live
    let all = dataset.groups["all"]
        .vars
        .as_ref()
        .expect("all carries vars");
    assert_eq!(all["ntp"], "pool.ntp");
    assert_eq!(all["useransible"], "pq_ansible");
    assert!(!all.contains_key("ignored"));
    // and group_vars/web/ on web
    assert_eq!(
        dataset.groups["web"].vars.as_ref().unwrap()["http_port"],
        8080
    );
    // host_vars/<host>/ is the host's own
    assert_eq!(dataset.hostvars["web01.example.com"]["disk"], "ssd");
}

// A key set in two files of the same directory takes the later one, matching
// Ansible's alphabetical merge — and not whatever order the filesystem gave.
#[tokio::test]
async fn files_in_a_vars_directory_merge_alphabetically() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("inventory.yaml"),
        "all:\n  hosts:\n    solo.example.com: {}\n",
    )
    .await;
    write(&dir.path().join("group_vars/all/a_first.yaml"), "who: a\n").await;
    write(&dir.path().join("group_vars/all/z_last.yaml"), "who: z\n").await;

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
        .expect("must parse")
        .dataset;

    assert_eq!(dataset.groups["all"].vars.as_ref().unwrap()["who"], "z");
}

// Both layouts for one name is ambiguous. Picking one would give a variable a
// value nobody can find by reading the tree, so it is refused instead.
#[tokio::test]
async fn a_name_defined_as_both_file_and_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("inventory.yaml"),
        "all:\n  hosts:\n    solo.example.com: {}\n",
    )
    .await;
    write(&dir.path().join("group_vars/all.yaml"), "who: file\n").await;
    write(&dir.path().join("group_vars/all/x.yaml"), "who: dir\n").await;

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
        .expect_err("both layouts for one name must be refused");

    assert!(
        err.message.contains("both as a file and as a directory"),
        "error was: {}",
        err.message
    );
}
