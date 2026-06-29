use std::collections::HashMap;
use unified_api::adapters::process_connector::ProcessConnector;
use unified_api::ports::connector::ConnectorPort;

// Helper: construye un config con el escenario deseado
fn config_with_scenario(scenario: &str) -> HashMap<String, String> {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), scenario.to_string());
    config
}

// Credenciales vacías (el fake connector no las necesita)
fn empty_credentials() -> HashMap<String, String> {
    HashMap::new()
}

// =========================================================================
// Test: escenario default — inventario con 3 hosts
// =========================================================================
#[tokio::test]
async fn execute_default_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "test-connectors/fake_inventory.py",
            &config_with_scenario("default"),
            &empty_credentials(),
        )
        .await;

    // Verificamos que no falló
    assert!(result.is_ok(), "Connector failed: {:?}", result.err());

    let dataset = result.unwrap();

    // 6 hosts en el inventario default: 3 en Section 9, 3 MAGI en SEELE
    assert_eq!(dataset.hostvars.len(), 6);
    assert!(dataset.hostvars.contains_key("motoko.section9.net"));
    assert!(dataset.hostvars.contains_key("batou.section9.net"));
    assert!(dataset.hostvars.contains_key("tachikoma01.section9.net"));
    assert!(dataset.hostvars.contains_key("melchior.seele.net"));
    assert!(dataset.hostvars.contains_key("balthasar.seele.net"));
    assert!(dataset.hostvars.contains_key("casper.seele.net"));

    // Verificamos que los grupos existen
    assert!(dataset.groups.contains_key("section9"));
    assert!(dataset.groups.contains_key("seele"));
    assert!(dataset.groups.contains_key("magi"));
    assert!(dataset.groups.contains_key("production"));
}

// =========================================================================
// Test: escenario empty — inventario vacío
// =========================================================================
#[tokio::test]
async fn execute_empty_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "test-connectors/fake_inventory.py",
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
// Test: escenario large — 50 hosts
// =========================================================================
#[tokio::test]
async fn execute_large_inventory() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "test-connectors/fake_inventory.py",
            &config_with_scenario("large"),
            &empty_credentials(),
        )
        .await;

    assert!(result.is_ok());

    let dataset = result.unwrap();
    assert_eq!(dataset.hostvars.len(), 50);
    assert!(dataset.groups.contains_key("production"));
    assert!(dataset.groups.contains_key("staging"));

    // production tiene 25 hosts, staging tiene 25
    let prod = dataset.groups.get("production").unwrap();
    assert_eq!(prod.hosts.len(), 25);
}

// =========================================================================
// Test: escenario error — el script falla con exit code 1
// =========================================================================
#[tokio::test]
async fn execute_error_scenario() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "test-connectors/fake_inventory.py",
            &config_with_scenario("error"),
            &empty_credentials(),
        )
        .await;

    // Debe ser un error
    assert!(result.is_err());

    let error = result.unwrap_err();
    assert_eq!(error.exit_code, Some(1));
    assert!(error.stderr.contains("Could not connect"));
}

// =========================================================================
// Test: script que no existe — error de ejecución
// =========================================================================
#[tokio::test]
async fn execute_nonexistent_script() {
    let connector = ProcessConnector::new();

    let result = connector
        .execute(
            "test-connectors/this_does_not_exist.py",
            &HashMap::new(),
            &empty_credentials(),
        )
        .await;

    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.message.contains("Failed to execute"));
}

// =========================================================================
// Test: credenciales se pasan como env vars
// =========================================================================
#[tokio::test]
async fn credentials_are_passed_as_env_vars() {
    let connector = ProcessConnector::new();

    let mut credentials = HashMap::new();
    credentials.insert("username".to_string(), "admin".to_string());
    credentials.insert("password".to_string(), "secret123".to_string());

    // El fake connector no usa las credenciales, pero al menos
    // verificamos que no crashea al pasarlas
    let result = connector
        .execute(
            "test-connectors/fake_inventory.py",
            &config_with_scenario("default"),
            &credentials,
        )
        .await;

    assert!(result.is_ok());
}
