use std::collections::HashMap;
use unified_api::adapters::out::connectors::process::ProcessConnector;
use unified_api::domain::source::OutputFormat;
use unified_api::ports::connector::ConnectorPort;

// Helper: builds a config with the desired scenario
fn config_with_scenario(scenario: &str) -> HashMap<String, String> {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), scenario.to_string());
    config
}

// Empty credentials (the sample connector does not need them)
fn empty_credentials() -> HashMap<String, String> {
    HashMap::new()
}

// =========================================================================
// Test: default scenario — inventory with 3 hosts
// =========================================================================
#[tokio::test]
async fn execute_default_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/inventory.py",
            &[],
            OutputFormat::Native,
            &config_with_scenario("default"),
            &empty_credentials(),
        )
        .await;

    // Verify it did not fail
    assert!(result.is_ok(), "Connector failed: {:?}", result.err());

    let dataset = result.unwrap();

    // 6 hosts in default inventory: 3 in Section 9, 3 MAGI in SEELE
    assert_eq!(dataset.hostvars.len(), 6);
    assert!(dataset.hostvars.contains_key("motoko.section9.net"));
    assert!(dataset.hostvars.contains_key("batou.section9.net"));
    assert!(dataset.hostvars.contains_key("tachikoma01.section9.net"));
    assert!(dataset.hostvars.contains_key("melchior.seele.net"));
    assert!(dataset.hostvars.contains_key("balthasar.seele.net"));
    assert!(dataset.hostvars.contains_key("casper.seele.net"));

    // Verify the groups exist
    assert!(dataset.groups.contains_key("section9"));
    assert!(dataset.groups.contains_key("seele"));
    assert!(dataset.groups.contains_key("magi"));
    assert!(dataset.groups.contains_key("production"));
}

// =========================================================================
// Test: empty scenario — empty inventory
// =========================================================================
#[tokio::test]
async fn execute_empty_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/inventory.py",
            &[],
            OutputFormat::Native,
            &config_with_scenario("empty"),
            &empty_credentials(),
        )
        .await;

    assert!(result.is_ok());

    let dataset = result.unwrap();
    assert_eq!(dataset.hostvars.len(), 0);
    assert_eq!(dataset.groups.len(), 0);
}

// =========================================================================
// Test: large scenario — 50 hosts
// =========================================================================
#[tokio::test]
async fn execute_large_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/inventory.py",
            &[],
            OutputFormat::Native,
            &config_with_scenario("large"),
            &empty_credentials(),
        )
        .await;

    assert!(result.is_ok());

    let dataset = result.unwrap();
    assert_eq!(dataset.hostvars.len(), 50);
    assert!(dataset.groups.contains_key("production"));
    assert!(dataset.groups.contains_key("staging"));

    // production has 25 hosts, staging has 25
    let prod = dataset.groups.get("production").unwrap();
    assert_eq!(prod.hosts.len(), 25);
}

// =========================================================================
// Test: error scenario — script fails with exit code 1
// =========================================================================
#[tokio::test]
async fn execute_error_scenario() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/inventory.py",
            &[],
            OutputFormat::Native,
            &config_with_scenario("error"),
            &empty_credentials(),
        )
        .await;

    // Must be an error
    assert!(result.is_err());

    let error = result.unwrap_err();
    assert_eq!(error.exit_code, Some(1));
    assert!(error.stderr.contains("Could not connect"));
}

// =========================================================================
// Test: nonexistent script — execution error
// =========================================================================
#[tokio::test]
async fn execute_nonexistent_script() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/this_does_not_exist.py",
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.message.contains("Failed to execute"));
}

// =========================================================================
// Test: credentials are passed as env vars
// =========================================================================
#[tokio::test]
async fn credentials_are_passed_as_env_vars() {
    let connector = ProcessConnector::new();

    let mut credentials = HashMap::new();
    credentials.insert("username".to_string(), "admin".to_string());
    credentials.insert("password".to_string(), "secret123".to_string());

    // The sample connector does not use credentials, but at least
    // we verify it does not crash when passed them
    let result = connector
        .execute(
            "tests/adapters/out/connectors/inventory.py",
            &[],
            OutputFormat::Native,
            &config_with_scenario("default"),
            &credentials,
        )
        .await;

    assert!(result.is_ok());
}

// =========================================================================
// ProcessOutput: runs an output transformer script over cached datasets
// =========================================================================
mod output {
    use std::collections::HashMap;
    use unified_api::adapters::out::output::process::ProcessOutput;
    use unified_api::domain::dataset::Dataset;
    use unified_api::ports::output::OutputPort;

    fn dataset_with_host(host: &str) -> Dataset {
        let mut hostvars = HashMap::new();
        let mut vars = HashMap::new();
        vars.insert("os".to_string(), serde_json::json!("OracleLinux"));
        hostvars.insert(host.to_string(), vars);
        Dataset {
            hostvars,
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    #[tokio::test]
    async fn output_transforms_datasets_to_ansible_inventory() {
        let output = ProcessOutput::new();

        let mut datasets = HashMap::new();
        datasets.insert(
            "src-a".to_string(),
            dataset_with_host("motoko.section9.net"),
        );

        let result = output
            .execute(
                "tests/adapters/out/output/ansible_inventory.py",
                &[],
                &HashMap::new(),
                &serde_json::json!({}),
                &datasets,
            )
            .await;

        assert!(result.is_ok(), "output script failed: {:?}", result.err());
        let inventory: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        // Ansible inventory format: hostvars live under _meta
        assert!(inventory["_meta"]["hostvars"]["motoko.section9.net"].is_object());
    }

    #[tokio::test]
    async fn output_reports_error_for_missing_script() {
        let output = ProcessOutput::new();
        let result = output
            .execute(
                "tests/adapters/out/connectors/does_not_exist.py",
                &[],
                &HashMap::new(),
                &serde_json::json!({}),
                &HashMap::new(),
            )
            .await;
        assert!(result.is_err());
    }
}

// =========================================================================
// Test: script_args — the Ansible dynamic inventory CLI convention
// =========================================================================

// args_list.py mimics an argparse-based inventory script: it demands --list.
// Without args it must fail with exit code 2, like real-world scripts do.
#[tokio::test]
async fn execute_without_required_args_fails() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/args_list.py",
            &[],
            OutputFormat::Native,
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    let err = result.expect_err("script requires --list, must fail without it");
    assert_eq!(err.exit_code, Some(2));
}

#[tokio::test]
async fn execute_passes_script_args_verbatim() {
    let connector = ProcessConnector::new();

    let args = vec!["--list".to_string(), "--refresh".to_string()];
    let result = connector
        .execute(
            "tests/adapters/out/connectors/args_list.py",
            &args,
            OutputFormat::Native,
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    let dataset = result.expect("connector must succeed with --list");
    // The fixture echoes back the argv it received as a hostvar
    let received = &dataset.hostvars["argshost.section9.net"]["received_args"];
    assert_eq!(received, &serde_json::json!(["--list", "--refresh"]));
}

// =========================================================================
// Test: output_format ansible — standard dynamic inventory JSON conversion
// =========================================================================

#[tokio::test]
async fn ansible_format_output_is_converted_to_dataset() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/ansible_inventory_source.py",
            &["--list".to_string()],
            OutputFormat::Ansible,
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    let dataset = result.expect("ansible-format connector must succeed");

    // hostvars extracted from _meta.hostvars
    assert_eq!(dataset.hostvars.len(), 2);
    assert_eq!(
        dataset.hostvars["motoko.section9.net"]["ansible_host"],
        "10.9.1.1"
    );
    // groups from top-level keys — object form and legacy list form,
    // with the implicit all/ungrouped meta-groups skipped
    assert_eq!(dataset.groups.len(), 2);
    assert_eq!(dataset.groups["section9"].hosts.len(), 2);
    assert_eq!(dataset.groups["legacy"].hosts, vec!["motoko.section9.net"]);
    assert!(!dataset.groups.contains_key("all"));
    assert!(!dataset.groups.contains_key("ungrouped"));
}

// The bug this feature fixes: ansible JSON parsed as native format "succeeds"
// with an empty dataset because both Dataset fields default. The connector now
// logs a WARN suggesting output_format: ansible — the data outcome stays the
// same (this test documents it).
#[tokio::test]
async fn ansible_output_parsed_as_native_yields_empty_dataset() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "tests/adapters/out/connectors/ansible_inventory_source.py",
            &["--list".to_string()],
            OutputFormat::Native,
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    let dataset = result.expect("parse succeeds — that is precisely the trap");
    assert!(dataset.hostvars.is_empty());
    assert!(dataset.groups.is_empty());
}
