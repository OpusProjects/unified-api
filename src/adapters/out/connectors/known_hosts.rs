use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey, parse_public_key_base64};
use sha1::Sha1;
use tracing::warn;

// An OpenSSH known_hosts file, parsed once per sync and shared by every
// per-host connection of that sync (an Arc clone per host, not a file read
// per host — a fleet source dials thousands of hosts per gather).
//
// Supported line shapes, matching what `ssh-keyscan` and OpenSSH write:
//   hostname[,hostname2,...] keytype base64-key [comment]
//   [hostname]:port keytype base64-key            (non-22 ports)
//   |1|salt|hash keytype base64-key               (HashKnownHosts entries)
// Comment lines, blank lines and unparsable lines are skipped with a
// warning. `@cert-authority`/`@revoked` markers are not supported and are
// also skipped — which fails CLOSED: a key that is only recorded behind a
// marker is simply not recorded, so the host is refused as unknown.
pub(crate) struct KnownHosts {
    entries: Vec<(String, PublicKey)>,
}

// What comparing an offered server key against the file amounts to.
pub(crate) enum HostKeyCheck {
    // The offered key matches a recorded entry for this host.
    Known,
    // The host has recorded entries but the offered key matches none of
    // them — the alarming case. Carries the recorded keys' fingerprints so
    // the refusal can name what was expected.
    Changed { expected: Vec<String> },
    // No entry for this host at all.
    Unknown,
}

impl KnownHosts {
    pub(crate) fn parse(content: &str) -> Self {
        let mut entries = Vec::new();
        for (index, raw) in content.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('@') {
                warn!(
                    line = index + 1,
                    "known_hosts marker lines (@cert-authority/@revoked) are not \
                     supported; entry ignored"
                );
                continue;
            }
            let mut fields = line.split_whitespace();
            let (Some(pattern), Some(_keytype), Some(key)) =
                (fields.next(), fields.next(), fields.next())
            else {
                warn!(line = index + 1, "malformed known_hosts line ignored");
                continue;
            };
            match parse_public_key_base64(key) {
                Ok(parsed) => entries.push((pattern.to_string(), parsed)),
                Err(e) => {
                    warn!(line = index + 1, error = %e, "unparsable key in known_hosts ignored");
                }
            }
        }
        Self { entries }
    }

    pub(crate) fn check(&self, host: &str, port: u16, offered: &PublicKey) -> HostKeyCheck {
        // known_hosts records non-standard ports as `[host]:port`
        let host_port = if port == 22 {
            host.to_string()
        } else {
            format!("[{host}]:{port}")
        };

        let recorded: Vec<&PublicKey> = self
            .entries
            .iter()
            .filter(|(pattern, _)| pattern_matches(&host_port, pattern))
            .map(|(_, key)| key)
            .collect();

        if recorded.is_empty() {
            return HostKeyCheck::Unknown;
        }
        // A host may legitimately have several keys (one per algorithm);
        // any exact match accepts. A recorded key of a different algorithm
        // does not: OpenSSH is equally strict, and "the server suddenly
        // offers an algorithm we never recorded" is not distinguishable
        // from a MITM downgrade.
        if recorded.contains(&offered) {
            return HostKeyCheck::Known;
        }
        HostKeyCheck::Changed {
            expected: recorded
                .iter()
                .map(|key| key.fingerprint(HashAlg::Sha256).to_string())
                .collect(),
        }
    }
}

// One pattern field may hold several comma-separated names, each either
// plain or hashed (`|1|base64-salt|base64-hmac`, where the HMAC-SHA1 of the
// hostname under the salt must equal the recorded hash).
fn pattern_matches(host_port: &str, pattern: &str) -> bool {
    pattern
        .split(',')
        .any(|entry| match entry.strip_prefix("|1|") {
            Some(hashed) => hashed_matches(host_port, hashed),
            None => entry == host_port,
        })
}

fn hashed_matches(host_port: &str, salt_and_hash: &str) -> bool {
    let mut parts = salt_and_hash.split('|');
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (BASE64.decode(salt), BASE64.decode(hash)) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(host_port.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two real ed25519 public keys (generated for these tests, used nowhere)
    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOFenJYuLbBYxI8THkzUY5pQJE3qhxscJmos8GwrwMF+";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIKz+d7teqxdefYuytF921PeNhFlO0SDtXy57BMwsJWLh";
    // `ssh-keyscan -H`-style entry: web01.example.com hashed, holding KEY_A
    const HASHED_WEB01: &str = "|1|5bt7fgbPOjfHRMLfmdH5y+KW2rg=|nQgv30f1EF9gbZ7Zp4szhSpKcJw= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOFenJYuLbBYxI8THkzUY5pQJE3qhxscJmos8GwrwMF+";

    fn key(base64: &str) -> PublicKey {
        parse_public_key_base64(base64).unwrap()
    }

    #[test]
    fn a_recorded_key_is_known() {
        let kh = KnownHosts::parse(&format!("web01.example.com ssh-ed25519 {KEY_A}\n"));
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Known
        ));
    }

    #[test]
    fn a_different_key_for_a_recorded_host_is_changed_and_names_the_expected() {
        let kh = KnownHosts::parse(&format!("web01.example.com ssh-ed25519 {KEY_A}\n"));
        match kh.check("web01.example.com", 22, &key(KEY_B)) {
            HostKeyCheck::Changed { expected } => {
                assert_eq!(expected.len(), 1);
                assert!(expected[0].starts_with("SHA256:"), "was: {}", expected[0]);
            }
            _ => panic!("a mismatched key must be Changed, not accepted or unknown"),
        }
    }

    #[test]
    fn a_host_without_an_entry_is_unknown() {
        let kh = KnownHosts::parse(&format!("web01.example.com ssh-ed25519 {KEY_A}\n"));
        assert!(matches!(
            kh.check("other.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        ));
    }

    #[test]
    fn a_non_standard_port_needs_the_bracketed_entry() {
        let kh = KnownHosts::parse(&format!("[web01.example.com]:2222 ssh-ed25519 {KEY_A}\n"));
        assert!(matches!(
            kh.check("web01.example.com", 2222, &key(KEY_A)),
            HostKeyCheck::Known
        ));
        // the same entry does not cover port 22
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        ));
    }

    #[test]
    fn a_comma_separated_alias_list_matches_each_name() {
        let kh = KnownHosts::parse(&format!("web01,web01.example.com ssh-ed25519 {KEY_A}\n"));
        assert!(matches!(
            kh.check("web01", 22, &key(KEY_A)),
            HostKeyCheck::Known
        ));
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Known
        ));
    }

    #[test]
    fn a_hashed_entry_matches_its_hostname() {
        let kh = KnownHosts::parse(HASHED_WEB01);
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Known
        ));
        // the hash is of the hostname, so any other name misses
        assert!(matches!(
            kh.check("web02.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        ));
    }

    #[test]
    fn several_keys_for_one_host_accept_any_match() {
        let kh = KnownHosts::parse(&format!(
            "web01.example.com ssh-ed25519 {KEY_A}\nweb01.example.com ssh-ed25519 {KEY_B}\n"
        ));
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_B)),
            HostKeyCheck::Known
        ));
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        let kh = KnownHosts::parse(&format!(
            "# a comment\n\nnot a valid line\n\
             @revoked web09 ssh-ed25519 {KEY_B}\n\
             web01.example.com ssh-ed25519 not-base64!!\n\
             web01.example.com ssh-ed25519 {KEY_A}\n"
        ));
        assert!(matches!(
            kh.check("web01.example.com", 22, &key(KEY_A)),
            HostKeyCheck::Known
        ));
        // the @revoked marker line is ignored, so its host is unknown → refused
        assert!(matches!(
            kh.check("web09", 22, &key(KEY_B)),
            HostKeyCheck::Unknown
        ));
    }
}
