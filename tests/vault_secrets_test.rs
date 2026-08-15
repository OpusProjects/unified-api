// The Vault adapter against a fake Vault: an in-process axum server speaking
// just enough KV v2 (and kubernetes login) to exercise every path — auth,
// mapping, fallback, and the failure shapes. No real Vault, no network.
use std::collections::HashMap;

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use unified_api::adapters::out::secrets::env::EnvSecrets;
use unified_api::adapters::out::secrets::vault::{VaultConfig, VaultSecrets};
use unified_api::domain::credential::Credential;
use unified_api::ports::secrets::SecretsPort;

async fn fake_vault() -> String {
    let app = Router::new()
        .route(
            "/v1/secret/data/{*path}",
            get(|Path(path): Path<String>, headers: HeaderMap| async move {
                if headers
                    .get("x-vault-token")
                    .and_then(|value| value.to_str().ok())
                    != Some("good-token")
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({"errors": ["permission denied"]})),
                    );
                }
                if path == "team/api" {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "data": {
                                "data": {"user": "motoko", "pass": "puppetmaster", "port": 8443},
                                "metadata": {"version": 1}
                            }
                        })),
                    )
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"errors": []})),
                    )
                }
            }),
        )
        .route(
            "/v1/auth/kubernetes/login",
            post(|Json(body): Json<serde_json::Value>| async move {
                if body["role"] == "unified-api" && body["jwt"] == "sa-jwt" {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "auth": {"client_token": "good-token", "lease_duration": 3600}
                        })),
                    )
                } else {
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({"errors": ["bad role or jwt"]})),
                    )
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

fn credential(yaml: &str) -> Credential {
    serde_yaml_ng::from_str(yaml).expect("credential fixture")
}

fn credentials() -> HashMap<String, Credential> {
    let mut map = HashMap::new();
    map.insert(
        "cred-vault".to_string(),
        credential(
            "name: Vault\ntype: token\nvault_path: \"team/api\"\nsecret_keys:\n  username: \"user\"\n  password: \"pass\"\n",
        ),
    );
    map.insert(
        "cred-vault-all".to_string(),
        credential("name: VaultAll\ntype: token\nvault_path: \"team/api\"\n"),
    );
    map.insert(
        "cred-vault-ghost".to_string(),
        credential("name: Ghost\ntype: token\nvault_path: \"team/nowhere\"\n"),
    );
    map
}

fn vault_secrets(config_yaml: &str, mut creds: HashMap<String, Credential>) -> VaultSecrets {
    // The fallback sees the same credential map, exactly as in main
    for (id, cred) in credentials() {
        creds.entry(id).or_insert(cred);
    }
    let config: VaultConfig = serde_yaml_ng::from_str(config_yaml).expect("vault config fixture");
    VaultSecrets::new(config, creds.clone(), Box::new(EnvSecrets::new(creds)))
}

#[tokio::test]
async fn token_auth_resolves_and_maps_secret_keys() {
    let address = fake_vault().await;
    // set_var is unsafe in edition 2024 (it races other threads reading the
    // environment); a test-unique name keeps it harmless here
    unsafe { std::env::set_var("VAULT_TEST_TOKEN_MAP", "good-token") };

    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\ntoken_env: \"VAULT_TEST_TOKEN_MAP\"\n",
            address
        ),
        HashMap::new(),
    );

    let secrets = vault.resolve("cred-vault").await.expect("resolves");
    assert_eq!(secrets["username"], "motoko");
    assert_eq!(secrets["password"], "puppetmaster");
    assert!(
        !secrets.contains_key("port"),
        "unmapped fields stay out when secret_keys is declared"
    );
}

#[tokio::test]
async fn empty_secret_keys_take_the_whole_secret() {
    let address = fake_vault().await;
    unsafe { std::env::set_var("VAULT_TEST_TOKEN_ALL", "good-token") };

    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\ntoken_env: \"VAULT_TEST_TOKEN_ALL\"\n",
            address
        ),
        HashMap::new(),
    );

    let secrets = vault.resolve("cred-vault-all").await.expect("resolves");
    assert_eq!(secrets["user"], "motoko");
    assert_eq!(
        secrets["port"], "8443",
        "non-strings arrive as their JSON rendering"
    );
}

#[tokio::test]
async fn a_wrong_token_is_a_named_failure() {
    let address = fake_vault().await;
    unsafe { std::env::set_var("VAULT_TEST_TOKEN_BAD", "wrong-token") };

    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\ntoken_env: \"VAULT_TEST_TOKEN_BAD\"\n",
            address
        ),
        HashMap::new(),
    );

    let err = vault
        .resolve("cred-vault")
        .await
        .expect_err("403 from vault");
    assert!(err.message.contains("403"), "error was: {}", err.message);
    assert!(
        err.message.contains("team/api"),
        "error was: {}",
        err.message
    );
}

#[tokio::test]
async fn a_missing_secret_names_the_path() {
    let address = fake_vault().await;
    unsafe { std::env::set_var("VAULT_TEST_TOKEN_GHOST", "good-token") };

    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\ntoken_env: \"VAULT_TEST_TOKEN_GHOST\"\n",
            address
        ),
        HashMap::new(),
    );

    let err = vault
        .resolve("cred-vault-ghost")
        .await
        .expect_err("404 from vault");
    assert!(
        err.message.contains("team/nowhere"),
        "error was: {}",
        err.message
    );
}

// The migration story: a credential WITHOUT a vault_path keeps resolving from
// env/files through the inner adapter, so Vault adoption is per credential.
#[tokio::test]
async fn a_credential_without_vault_path_falls_through_to_env() {
    let address = fake_vault().await;

    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), br#"{"token": "from-a-file"}"#).unwrap();

    let mut creds = HashMap::new();
    creds.insert(
        "cred-file".to_string(),
        credential(&format!(
            "name: File\ntype: token\nsecret_file: \"{}\"\nsecret_keys:\n  token: \"token\"\n",
            secret_file.path().display()
        )),
    );

    // No token env var set at all: the fallback path must not need Vault auth
    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\ntoken_env: \"VAULT_TEST_TOKEN_UNSET\"\n",
            address
        ),
        creds,
    );

    let secrets = vault
        .resolve("cred-file")
        .await
        .expect("resolves via env adapter");
    assert_eq!(secrets["token"], "from-a-file");
}

#[tokio::test]
async fn kubernetes_auth_logs_in_with_the_service_account_jwt() {
    let address = fake_vault().await;

    let jwt = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(jwt.path(), "sa-jwt\n").unwrap();

    let vault = vault_secrets(
        &format!(
            "address: \"{}\"\nkubernetes_role: \"unified-api\"\njwt_path: \"{}\"\n",
            address,
            jwt.path().display()
        ),
        HashMap::new(),
    );

    let secrets = vault.resolve("cred-vault").await.expect("login then read");
    assert_eq!(secrets["username"], "motoko");

    // A second resolve reuses the cached login (the fake would answer anyway;
    // the observable contract is simply that it still works)
    let again = vault.resolve("cred-vault").await.expect("cached login");
    assert_eq!(again["username"], "motoko");
}
