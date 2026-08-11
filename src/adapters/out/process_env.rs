use tokio::process::Command;

// Everything a spawned script keeps from the service's own environment.
//
// The process adapters used to hand children the FULL parent environment,
// which meant every connector script saw every API-key secret and every other
// source's `CREDENTIAL_*`-backing variables alongside the scoped set it was
// actually granted. Now the environment is cleared and rebuilt: this
// passthrough list first, then whatever the adapter injects explicitly
// (`SOURCE_CONFIG`, `CREDENTIAL_*`, `ENDPOINT_CONFIG`, `ENDPOINT_PARAMS`).
//
// The list is what scripts legitimately need from the host:
// - PATH: the `#!/usr/bin/env python3` shebang resolves the interpreter
//   through it, and so does anything the script spawns itself
// - HOME, TMPDIR: tools that read `~/.config` or write temporary files
// - LANG, LC_ALL, TZ: locale and timezone
// - PYTHONPATH: shared in-house libraries for connector scripts
// - proxy and CA-bundle variables: connectors reach their sources over
//   HTTPS, often through a corporate proxy or against a private CA
const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "PYTHONPATH",
    // requests/urllib honour the lowercase forms, curl honours both;
    // pass through whichever the operator set
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
];

/// A `Command` whose environment is the passthrough list and nothing else.
/// Every adapter that spawns a local script builds its command here, so
/// "what does a child process see" has exactly one answer in the codebase.
pub(crate) fn scrubbed_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear();
    for var in ENV_PASSTHROUGH {
        // var_os, not var: pass a value through even if it is not valid UTF-8
        if let Some(value) = std::env::var_os(var) {
            cmd.env(var, value);
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    // `env(1)` with no arguments prints its environment — exactly the view a
    // spawned script has.
    #[tokio::test]
    async fn a_scrubbed_command_does_not_leak_the_parent_environment() {
        // set_var is `unsafe` in edition 2024 because other threads may read
        // the environment concurrently; a uniquely named test variable keeps
        // this harmless.
        unsafe { std::env::set_var("UNIFIED_API_TEST_PARENT_SECRET", "leaked") };

        let output = scrubbed_command("/usr/bin/env").output().await.unwrap();
        let env_dump = String::from_utf8_lossy(&output.stdout).to_string();

        assert!(
            !env_dump.contains("UNIFIED_API_TEST_PARENT_SECRET"),
            "the child saw a variable that is not on the passthrough list"
        );
        // and the scrub kept what scripts legitimately need
        assert!(env_dump.contains("PATH="));
    }

    #[tokio::test]
    async fn explicitly_injected_variables_survive_the_scrub() {
        let mut cmd = scrubbed_command("/usr/bin/env");
        cmd.env("SOURCE_CONFIG", "{}");

        let output = cmd.output().await.unwrap();
        let env_dump = String::from_utf8_lossy(&output.stdout).to_string();

        assert!(env_dump.contains("SOURCE_CONFIG={}"));
    }
}
