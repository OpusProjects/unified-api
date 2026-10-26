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
    // connector whose path is relative; meaningless for ssh, whose path runs
    // on the remote host (refused there, like any setting that looks active
    // and is not). `project` + `path` means the same thing on every item that
    // has them — a file inside that project's checkout — which is why the
    // script's location is `path` here, as it is on a static_inventory source.
    #[serde(default)]
    pub project: Option<String>,

    // script: the file to run (relative to the project checkout, or absolute).
    // ssh: the remote command for `gather_mode: script`, or a label such as
    // "gather_facts" for the built-in gather modes.
    pub path: String,

    // CLI arguments, e.g. ["--list"] for an Ansible dynamic inventory script.
    // For ssh in `script` gather mode they are appended to the remote command.
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
            }
            ConnectorKind::Ssh => {
                if self.project.is_some() {
                    errors.push(format!(
                        "Connector '{id}' is an ssh connector and sets project — its \
                         path runs on the remote host, so no checkout is used"
                    ));
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
            "name: s\ntype: ssh\nproject: p\npath: gather_facts\nhost_args: [\"{target}\"]\noutput_format: ansible\n",
        );
        let errors = c.errors("s").join("\n");
        assert!(errors.contains("sets project"), "{errors}");
        assert!(errors.contains("host_args"), "{errors}");
        assert!(errors.contains("output_format"), "{errors}");
    }

    #[test]
    fn a_built_in_name_is_refused() {
        let c = connector("name: x\npath: /opt/x.py\n");
        assert!(c.errors("remote")[0].contains("built-in"));
    }
}
