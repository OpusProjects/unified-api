// SSH connector integration tests against an IN-PROCESS russh server — no
// sshd, no network beyond loopback, no fixtures outside this file. The same
// crate that gives the connector its client gives the tests their server.
//
// What is covered end to end: host key verification (the recorded key
// gathers; a mismatched or unknown key is refused BEFORE authentication and
// the host lands in `unreachable`, keeping its last known data upstream),
// the accept-any behaviour when no known_hosts is configured, and the two
// connector locations: a script here streamed to the host's interpreter,
// and files on the host read as facts.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh::keys::ssh_key;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};

use unified_api::adapters::out::connectors::ssh::SshConnector;
use unified_api::ports::connector::{ConnectorOutput, ConnectorPort};

// Throwaway ed25519 keys generated for these tests, used nowhere else.
const SERVER_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDhXpyWLi2wWMSPEx5M1GOaUCRN6ocbHCZqLPBsK8DBfgAAAJALsGiWC7Bo
lgAAAAtzc2gtZWQyNTUxOQAAACDhXpyWLi2wWMSPEx5M1GOaUCRN6ocbHCZqLPBsK8DBfg
AAAEBecxz3ZIEBWwCpvY3eQmpr+uBqXd7SvppNFYxl1dh3AeFenJYuLbBYxI8THkzUY5pQ
JE3qhxscJmos8GwrwMF+AAAACHNlcnZlci1hAQIDBAU=
-----END OPENSSH PRIVATE KEY-----
";

const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCAbvPBCfv9z+2Mlf7HpnborFYTssGe5bv7usZrcaznqQAAAJBGY6f/RmOn
/wAAAAtzc2gtZWQyNTUxOQAAACCAbvPBCfv9z+2Mlf7HpnborFYTssGe5bv7usZrcaznqQ
AAAEAGYVDfrqvQJMIUSZeYVFxPvopR5eDZnxHsXBkani1C54Bu88EJ+/3P7YyV/semduis
VhOywZ7lu/u6xmtxrOepAAAABmNsaWVudAECAwQFBgc=
-----END OPENSSH PRIVATE KEY-----
";

// A public key that is NOT the server's — what a known_hosts entry records
// when the host was reinstalled (or when someone is in the middle).
const OTHER_PUBLIC: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKz+d7teqxdefYuytF921PeNhFlO0SDtXy57BMwsJWLh";

// Accepts any public key auth. Without `echo`, answers every exec with one
// JSON object and a clean exit — the shape parse_host_output stores as host
// vars. With it, plays a host: a script on standard input is answered once
// the input ends, with the command and the script it got; the facts loop is
// answered with two facts files, one JSON and one INI.
struct TestServer {
    auth_attempted: Arc<AtomicBool>,
    // The server half of the session channel; dropping it would close the
    // channel under the client, so it is parked here for the handler's life.
    channel: Option<Channel<Msg>>,
    echo: bool,
    command: String,
    stdin: Vec<u8>,
}

impl TestServer {
    fn answer(
        &self,
        channel: ChannelId,
        body: Vec<u8>,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        session.data(channel, body)?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.auth_attempted.store(true, Ordering::SeqCst);
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        self.channel = Some(channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        if !self.echo {
            return self.answer(channel, b"{\"probe\": true}".to_vec(), session);
        }
        self.command = String::from_utf8_lossy(command).into_owned();
        if self.command.contains("for f in ") {
            let facts =
                "\u{1e}owner\n{\"team\": \"platform\"}\n\n\u{1e}rack\n[location]\nRow = 4\n\n";
            return self.answer(channel, facts.as_bytes().to_vec(), session);
        }
        // A script on standard input: answered when the input ends.
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stdin.extend_from_slice(data);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.echo {
            return Ok(());
        }
        let body = serde_json::json!({
            "command": self.command,
            "script": String::from_utf8_lossy(&self.stdin),
        });
        self.answer(channel, body.to_string().into_bytes(), session)
    }
}

// Bind on an ephemeral loopback port and serve connections until the test
// ends (the listener task dies with the runtime).
async fn start_server(auth_attempted: Arc<AtomicBool>) -> SocketAddr {
    serve(auth_attempted, false).await
}

async fn serve(auth_attempted: Arc<AtomicBool>, echo: bool) -> SocketAddr {
    let key = russh::keys::decode_secret_key(SERVER_KEY, None).unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        ..Default::default()
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let config = Arc::clone(&config);
            let handler = TestServer {
                auth_attempted: Arc::clone(&auth_attempted),
                channel: None,
                echo,
                command: String::new(),
                stdin: Vec::new(),
            };
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, socket, handler).await {
                    let _ = session.await;
                }
            });
        }
    });

    addr
}

// The server's real host key as the known_hosts line the operator would
// have collected with `ssh-keyscan -p <port> 127.0.0.1`.
fn real_key_line(addr: SocketAddr) -> String {
    let key = russh::keys::decode_secret_key(SERVER_KEY, None).unwrap();
    format!(
        "[127.0.0.1]:{} {}",
        addr.port(),
        key.public_key().to_openssh().unwrap()
    )
}

async fn gather(
    addr: SocketAddr,
    known_hosts_line: Option<String>,
) -> Result<ConnectorOutput, unified_api::ports::connector::ConnectorError> {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("client_key");
    std::fs::write(&key_path, CLIENT_KEY).unwrap();

    let mut config: HashMap<String, String> = HashMap::new();
    config.insert("hosts".into(), "127.0.0.1".into());
    config.insert("port".into(), addr.port().to_string());
    config.insert("gather_mode".into(), "script".into());
    config.insert("ssh_connect_timeout_seconds".into(), "10".into());
    if let Some(line) = known_hosts_line {
        let kh_path = dir.path().join("known_hosts");
        std::fs::write(&kh_path, line).unwrap();
        config.insert(
            "ssh_known_hosts".into(),
            kh_path.to_string_lossy().into_owned(),
        );
    }

    let mut credentials: HashMap<String, String> = HashMap::new();
    credentials.insert("username".into(), "tester".into());
    credentials.insert(
        "ssh_key_path".into(),
        key_path.to_string_lossy().into_owned(),
    );

    // The test's own deadline, so a handshake regression fails in seconds
    // instead of hanging the suite.
    tokio::time::timeout(
        Duration::from_secs(30),
        SshConnector::new().execute(
            "/remote/command/ignored/by/the/test/server",
            &[],
            &config,
            &credentials,
        ),
    )
    .await
    .expect("the SSH gather deadlocked")
}

#[tokio::test]
async fn the_recorded_host_key_gathers() {
    let auth = Arc::new(AtomicBool::new(false));
    let addr = start_server(Arc::clone(&auth)).await;

    let output = gather(addr, Some(real_key_line(addr)))
        .await
        .expect("a host whose key is recorded must gather");

    assert!(output.unreachable.is_empty(), "{:?}", output.unreachable);
    assert_eq!(output.dataset.hostvars["127.0.0.1"]["probe"], true);
    assert!(auth.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_mismatched_host_key_never_reaches_authentication() {
    let auth = Arc::new(AtomicBool::new(false));
    let addr = start_server(Arc::clone(&auth)).await;

    // known_hosts records a DIFFERENT key for this address
    let line = format!("[127.0.0.1]:{} {}", addr.port(), OTHER_PUBLIC);
    let output = gather(addr, Some(line))
        .await
        .expect("a refused host fails the host, not the sync");

    assert_eq!(output.unreachable, vec!["127.0.0.1".to_string()]);
    assert!(output.dataset.hostvars.is_empty());
    // The whole point: the impostor never saw an authentication attempt,
    // so it never saw a signature from our key.
    assert!(!auth.load(Ordering::SeqCst));
}

#[tokio::test]
async fn an_unrecorded_host_is_refused() {
    let auth = Arc::new(AtomicBool::new(false));
    let addr = start_server(Arc::clone(&auth)).await;

    // A valid file that simply has no entry for this host
    let line = format!("elsewhere.example.com {}", OTHER_PUBLIC);
    let output = gather(addr, Some(line))
        .await
        .expect("a refused host fails the host, not the sync");

    assert_eq!(output.unreachable, vec!["127.0.0.1".to_string()]);
    assert!(!auth.load(Ordering::SeqCst));
}

#[tokio::test]
async fn without_known_hosts_any_key_is_still_accepted() {
    let auth = Arc::new(AtomicBool::new(false));
    let addr = start_server(Arc::clone(&auth)).await;

    // Backwards compatibility: no ssh_known_hosts on the source keeps the
    // historical accept-any behaviour (loudly warned about in the logs).
    let output = gather(addr, None)
        .await
        .expect("an unverified gather must still work");

    assert!(output.unreachable.is_empty());
    assert_eq!(output.dataset.hostvars["127.0.0.1"]["probe"], true);
}

#[tokio::test]
async fn an_unreadable_known_hosts_file_fails_the_sync_loudly() {
    let auth = Arc::new(AtomicBool::new(false));
    let addr = start_server(Arc::clone(&auth)).await;

    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("client_key");
    std::fs::write(&key_path, CLIENT_KEY).unwrap();

    let mut config: HashMap<String, String> = HashMap::new();
    config.insert("hosts".into(), "127.0.0.1".into());
    config.insert("port".into(), addr.port().to_string());
    config.insert(
        "ssh_known_hosts".into(),
        dir.path().join("missing").to_string_lossy().into_owned(),
    );
    let mut credentials: HashMap<String, String> = HashMap::new();
    credentials.insert("username".into(), "tester".into());
    credentials.insert(
        "ssh_key_path".into(),
        key_path.to_string_lossy().into_owned(),
    );

    let err = SshConnector::new()
        .execute("cmd", &[], &config, &credentials)
        .await
        .expect_err("a sync that cannot verify anything must fail, not gather");
    assert!(err.message.contains("ssh_known_hosts"), "{}", err.message);
}

// A connector with a location, against the echoing host: the path and
// arguments it is given, and what came back.
// None: a connector that writes its script inline, handed over as the path.
async fn gather_at(location: Option<&str>, path: &str, args: &[String]) -> ConnectorOutput {
    let addr = serve(Arc::new(AtomicBool::new(false)), true).await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("client_key");
    std::fs::write(&key_path, CLIENT_KEY).unwrap();
    let mut config: HashMap<String, String> = HashMap::from([
        ("hosts".into(), "127.0.0.1".into()),
        ("port".into(), addr.port().to_string()),
        ("ssh_connect_timeout_seconds".into(), "10".into()),
    ]);
    if let Some(location) = location {
        config.insert(
            unified_api::ports::connector::SSH_LOCATION_CONFIG_KEY.into(),
            location.into(),
        );
    }
    let credentials: HashMap<String, String> = HashMap::from([
        ("username".into(), "tester".into()),
        (
            "ssh_key_path".into(),
            key_path.to_string_lossy().into_owned(),
        ),
    ]);
    tokio::time::timeout(
        Duration::from_secs(30),
        SshConnector::new().execute(path, args, &config, &credentials),
    )
    .await
    .expect("the SSH gather deadlocked")
    .expect("the gather succeeds")
}

#[tokio::test]
async fn a_script_here_is_streamed_to_the_interpreter_it_names() {
    // The sample script, as a connector with location: instance reads it.
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/adapters/out/connectors/ssh/osinfo.sh"
    );

    let output = gather_at(Some("instance"), script, &["--short".into(), "it's".into()]).await;

    let host = &output.dataset.hostvars["127.0.0.1"];
    // Under /bin/sh whatever the login shell, written to a temporary file
    // and run by the interpreter from #! with the arguments, quoted, after it.
    let command = host["command"].as_str().unwrap();
    assert!(command.starts_with("exec /bin/sh -c '"), "{command}");
    assert!(
        command.contains(r#"/bin/sh "$t" "$@" </dev/null"#),
        "{command}"
    );
    assert!(command.ends_with(r"sh '--short' 'it'\''s'"), "{command}");
    // The file itself, byte for byte, over the connection: nothing written
    // on the host.
    assert_eq!(host["script"], std::fs::read_to_string(script).unwrap());
}

#[tokio::test]
async fn a_missing_script_here_fails_the_sync_naming_it() {
    let addr = serve(Arc::new(AtomicBool::new(false)), true).await;
    let config: HashMap<String, String> = HashMap::from([
        ("hosts".into(), "127.0.0.1".into()),
        ("port".into(), addr.port().to_string()),
        (
            unified_api::ports::connector::SSH_LOCATION_CONFIG_KEY.into(),
            "instance".into(),
        ),
    ]);
    let err = SshConnector::new()
        .execute("/nowhere/osinfo.sh", &[], &config, &HashMap::new())
        .await
        .expect_err("no script, no sync");
    assert!(
        err.message.contains("/nowhere/osinfo.sh"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn files_on_the_host_become_facts_named_after_them() {
    let output = gather_at(Some("host"), "/etc/ansible/facts.d/*.fact", &[]).await;

    let host = &output.dataset.hostvars["127.0.0.1"];
    assert_eq!(host["owner"], serde_json::json!({"team": "platform"}));
    assert_eq!(host["rack"], serde_json::json!({"location": {"row": "4"}}));
    // Each fact is also a group, as before.
    assert!(output.dataset.groups.contains_key("rack"));
}

#[tokio::test]
async fn an_inline_script_is_streamed_like_a_file() {
    let script = "uname -r\nhostname\n";
    let output = gather_at(None, script, &["--all".into()]).await;

    let host = &output.dataset.hostvars["127.0.0.1"];
    // No #!: the account's login shell, as an ssh command line.
    let command = host["command"].as_str().unwrap();
    assert!(command.contains(r#""${SHELL:-/bin/sh}" "$t""#), "{command}");
    assert!(command.ends_with("sh '--all'"), "{command}");
    assert_eq!(host["script"], script);
}
