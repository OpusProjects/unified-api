use serde::Deserialize;

use super::source::OutputFormat;

// A connector says HOW to talk to a kind of system: which script to run, from
// which project, with which arguments, and how to read what it prints. A
// source says WHICH instance of that system (its credentials, its `config`,
// its schedule and TTLs) and names its connector. Ten Device42 tenants are
// ten sources and one connector; before connectors existed the script, its
// arguments and its output format were copied into each of the ten.
//
// Two connectors are built in and need no item: `remote` (another
// unified-api, see adapters/out/connectors/remote.rs) and `static_inventory`
// (an Ansible YAML inventory read from a project checkout). Neither has
// anything a user could configure about the HOW: the protocol is fixed, and
// the WHERE (the remote URL, the file in the checkout) is per source.
//
// `name` is the display label, as on every configuration item: the file's
// `description`, falling back to the identity (see config::take). The
// identity itself is the map key.
//
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Connector {
    pub name: String,

    // `type` in the file, which is a Rust keyword — hence the rename.
    #[serde(rename = "type", default)]
    pub kind: ConnectorKind,

    // Project whose checkout holds the script. Required for a script
    // connector whose path is relative, and for an ssh connector whose script
    // is in a project; refused on an ssh connector whose path is on the hosts.
    // `project` + `path` means the same thing on every item that has them —
    // a file inside that project's checkout — which is why the script's
    // location is `path` here, as it is on a static_inventory source.
    #[serde(default)]
    pub project: Option<String>,

    // ssh only: where `path` is. `host`: on each host, a file or a pattern
    // of files (/etc/ansible/facts.d/*.fact); each one is run if executable,
    // read if not, and becomes a fact named after it.
    // `instance`: a script on this instance (in `project`'s checkout, or on
    // its disk), streamed to each host and run there; the default when
    // `project` is set. Left out with no project: the inline form, where
    // `path` is the command itself and each source's `gather_mode` decides
    // (kept working, warned about at load).
    #[serde(default)]
    pub location: Option<Location>,

    // script: the file to run (relative to the project checkout, or absolute).
    // ssh: see `location`.
    pub path: String,

    // CLI arguments, e.g. ["--list"] for an Ansible dynamic inventory script.
    // ssh: passed to the script streamed from here, to every executable a
    // host path matches (files that are read ignore them), and appended to
    // an inline command.
    #[serde(default)]
    pub script_args: Vec<String>,

    // CLI arguments for a sync that names hosts, REPLACING script_args for
    // that run; `{target}` becomes the requested hostnames, comma-joined.
    // Script connectors only: ssh builds its own command per host.
    #[serde(default)]
    pub host_args: Vec<String>,

    // What the script prints (script connectors only — ssh builds its dataset
    // itself). Option so that setting it on ssh can be refused rather than
    // silently ignored; unset means native.
    #[serde(default)]
    pub output_format: Option<OutputFormat>,

    // Default bound on a sync through this connector. A source may override
    // it with its own timeout_seconds (a bigger tenant of the same system).
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    #[default]
    Script,
    Ssh,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    Host,
    Instance,
}

impl Location {
    pub fn name(self) -> &'static str {
        match self {
            Location::Host => "host",
            Location::Instance => "instance",
        }
    }
}

// What a shell pattern is made of: a script here is one file, so a path
// with one of these belongs on the hosts.
fn is_pattern(path: &str) -> bool {
    path.contains(['*', '?', '['])
}

// A path written into a remote shell command without quoting it, so a
// pattern still expands there: nothing that shell would read as anything but
// a path or a pattern.
fn is_plain_remote_path(path: &str) -> bool {
    path.starts_with('/')
        && path.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '/' | '.' | '_' | '-' | '+' | '@' | ':' | ',' | '*' | '?' | '[' | ']'
                )
        })
}

impl ConnectorKind {
    pub fn name(self) -> &'static str {
        match self {
            ConnectorKind::Script => "script",
            ConnectorKind::Ssh => "ssh",
        }
    }
}

// The built-in connectors, by the name a source writes after `connector:`.
pub const REMOTE: &str = "remote";
pub const STATIC_INVENTORY: &str = "static_inventory";
pub const BUILT_IN: [&str; 2] = [REMOTE, STATIC_INVENTORY];

impl Connector {
    // Where an ssh connector's path is; None for the inline form (and for
    // script connectors, which always run here).
    pub fn ssh_location(&self) -> Option<Location> {
        match self.kind {
            ConnectorKind::Script => None,
            ConnectorKind::Ssh => self
                .location
                .or(self.project.as_ref().map(|_| Location::Instance)),
        }
    }

    // What still works but should change: one line per connector, logged at
    // startup and printed by --check-config.
    pub fn warnings(&self, id: &str) -> Vec<String> {
        if self.kind == ConnectorKind::Ssh && self.ssh_location().is_none() {
            vec![format!(
                "Connector '{id}' writes its ssh command inline: set location: host with a \
                 path on the hosts (/etc/ansible/facts.d/*.fact), or put the script in a \
                 file and set project (or location: instance) — the inline form will be removed"
            )]
        } else {
            Vec::new()
        }
    }

    // Everything wrong with this connector on its own; references to other
    // items (the project) are checked by config validation.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if BUILT_IN.contains(&id) {
            errors.push(format!(
                "Connector '{id}' uses the name of a built-in connector — '{}' and '{}' \
                 need no item; rename this one",
                REMOTE, STATIC_INVENTORY
            ));
        }
        if self.path.trim().is_empty() {
            errors.push(format!("Connector '{id}' has an empty path"));
        }
        match self.kind {
            ConnectorKind::Script => {
                if self.project.is_none() && !std::path::Path::new(&self.path).is_absolute() {
                    errors.push(format!(
                        "Connector '{id}' has a relative path but no project — name \
                         the project whose checkout holds the script, or use an absolute path"
                    ));
                }
                if self.location.is_some() {
                    errors.push(format!(
                        "Connector '{id}' sets location, which only an ssh connector uses — \
                         a script connector always runs here"
                    ));
                }
            }
            ConnectorKind::Ssh => {
                match self.ssh_location() {
                    Some(Location::Host) => {
                        if self.project.is_some() {
                            errors.push(format!(
                                "Connector '{id}' sets project with location: host — its \
                                 path is on each host, so no checkout is used"
                            ));
                        }
                        if !is_plain_remote_path(&self.path) {
                            errors.push(format!(
                                "Connector '{id}' has location: host, so its path must be \
                                 absolute on the hosts and only letters, digits, / . _ - + @ \
                                 : , and the pattern characters * ? [ ] — got '{}'",
                                self.path
                            ));
                        }
                    }
                    Some(Location::Instance) => {
                        if self.path.contains('\n') {
                            errors.push(format!(
                                "Connector '{id}' has location: instance, so its path names a \
                                 script file; put the script's lines in that file"
                            ));
                        } else if is_pattern(&self.path) {
                            errors.push(format!(
                                "Connector '{id}' has location: instance but its path is a \
                                 pattern — a pattern reads files on each host: set location: host"
                            ));
                        }
                    }
                    None => {}
                }
                if !self.host_args.is_empty() {
                    errors.push(format!(
                        "Connector '{id}' sets host_args but is not a script connector"
                    ));
                }
                if self.output_format.is_some() {
                    errors.push(format!(
                        "Connector '{id}' sets output_format, which an ssh connector ignores \
                         — it builds its dataset itself"
                    ));
                }
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(yaml: &str) -> Connector {
        serde_saphyr::from_str(yaml).expect("connector fixture")
    }

    #[test]
    fn a_script_connector_defaults_to_native_output_and_the_shared_timeout() {
        let c = connector("name: d42\nproject: prj\npath: d42.py\n");
        assert_eq!(c.kind, ConnectorKind::Script);
        assert_eq!(c.output_format, None);
        assert_eq!(c.timeout_seconds, 300);
        assert!(c.errors("d42").is_empty());
    }

    #[test]
    fn a_relative_script_needs_a_project() {
        let c = connector("name: x\npath: x.py\n");
        assert!(c.errors("x")[0].contains("no project"));
        let c = connector("name: x\npath: /opt/x.py\n");
        assert!(c.errors("x").is_empty());
    }

    #[test]
    fn script_only_settings_are_refused_on_ssh() {
        let c = connector(
            "name: s\ntype: ssh\npath: gather_facts\nhost_args: [\"{target}\"]\noutput_format: ansible\n",
        );
        let errors = c.errors("s").join("\n");
        assert!(errors.contains("host_args"), "{errors}");
        assert!(errors.contains("output_format"), "{errors}");
    }

    #[test]
    fn an_ssh_path_is_on_the_hosts_or_a_script_here() {
        let facts =
            connector("name: f\ntype: ssh\nlocation: host\npath: /etc/ansible/facts.d/*.fact\n");
        assert_eq!(facts.ssh_location(), Some(Location::Host));
        assert!(facts.errors("f").is_empty(), "{:?}", facts.errors("f"));
        assert!(facts.warnings("f").is_empty());

        // A project means a script in its checkout, location: instance.
        let script = connector("name: s\ntype: ssh\nproject: p\npath: ssh/osinfo.sh\n");
        assert_eq!(script.ssh_location(), Some(Location::Instance));
        assert!(script.errors("s").is_empty(), "{:?}", script.errors("s"));

        // Neither: the inline form, still accepted, warned about.
        let inline = connector("name: i\ntype: ssh\npath: \"uname -r\"\n");
        assert_eq!(inline.ssh_location(), None);
        assert!(inline.errors("i").is_empty());
        assert!(inline.warnings("i")[0].contains("inline"));
    }

    #[test]
    fn a_path_on_the_hosts_is_absolute_and_plain() {
        for (path, why) in [
            ("etc/facts.d/*.fact", "relative"),
            ("/etc/facts.d/*.fact; rm -rf /", "a command"),
            ("/etc/$HOME/x", "an expansion"),
        ] {
            let c = connector(&format!(
                "name: f\ntype: ssh\nlocation: host\npath: \"{path}\"\n"
            ));
            assert!(
                c.errors("f").join("").contains("absolute on the hosts"),
                "{why}: {:?}",
                c.errors("f")
            );
        }
        let with_project =
            connector("name: f\ntype: ssh\nlocation: host\nproject: p\npath: /opt/x\n");
        assert!(with_project.errors("f").join("").contains("no checkout"));
    }

    #[test]
    fn a_script_here_is_a_file_not_lines_or_a_pattern() {
        let lines =
            connector("name: s\ntype: ssh\nlocation: instance\npath: \"uname -r\\nhostname\"\n");
        assert!(lines.errors("s").join("").contains("script file"));
        let pattern = connector("name: s\ntype: ssh\nproject: p\npath: \"ssh/*.sh\"\n");
        assert!(pattern.errors("s").join("").contains("set location: host"));
        let script = connector("name: s\ntype: script\nlocation: host\npath: /opt/x.py\n");
        assert!(
            script
                .errors("s")
                .join("")
                .contains("only an ssh connector")
        );
    }

    #[test]
    fn a_built_in_name_is_refused() {
        let c = connector("name: x\npath: /opt/x.py\n");
        assert!(c.errors("remote")[0].contains("built-in"));
    }
}
