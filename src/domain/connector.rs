use serde::Deserialize;

// A connector says HOW to talk to a kind of system: which script to run, from
// which project, with which arguments. A source says WHICH instance of that
// system (its credentials, its `config`, its schedule and TTLs) and names its
// connector. Ten Device42 tenants are ten sources and one connector; before
// connectors existed the script and its arguments were copied into each of
// the ten.
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
    // read if not, and becomes a fact named after it. `instance` (the
    // default with a project): a script on this instance, in `project`'s
    // checkout or on its disk, streamed to each host and run there. An ssh
    // connector with neither this, a project nor a script is in the 0.35
    // form, refused with what to write instead.
    #[serde(default)]
    pub location: Option<Location>,

    // A file, always: script: the one to run (relative to the project
    // checkout, or absolute); ssh: see `location`. An ssh connector has this
    // or `script`, never both.
    #[serde(default)]
    pub path: String,

    // ssh only: the script itself, written in the item, streamed to each
    // host and run there as a file from here would be — the inline choice
    // beside `path`, as a CI step is either inline or a file.
    #[serde(default)]
    pub script: Option<String>,

    // CLI arguments, e.g. ["--list"] for an Ansible dynamic inventory script.
    // ssh: the streamed script's arguments ($1, $2…, a file's or an inline
    // script's alike), or every executable a host path matches (files that
    // are read ignore them).
    #[serde(default)]
    pub script_args: Vec<String>,

    // CLI arguments for a sync that names hosts, REPLACING script_args for
    // that run; `{target}` becomes the requested hostnames, comma-joined.
    // Script connectors only: ssh builds its own command per host.
    #[serde(default)]
    pub host_args: Vec<String>,

    // What 0.35 asked: a script prints Ansible inventory JSON, the one
    // format 0.36 reads. `ansible` says exactly that and is accepted, so a
    // configuration that carries it works on 0.35 and 0.36 alike while
    // instances are upgraded one by one (removing it first would make a 0.35
    // instance read its scripts as `native`: an empty inventory). `native`
    // is the format that is gone, refused by name; anything else too.
    #[serde(default)]
    pub output_format: Option<String>,

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

// The key under which an ssh source's location (`host` or `instance`)
// reaches the ssh adapter in its config, set by application::sync. Absent: a
// connector that writes its script inline, handed over as the path. A source
// may not set it itself (SourceDef::resolve refuses it).
pub const SSH_LOCATION_CONFIG_KEY: &str = "ssh_location";

// A value as one shell word, whatever it holds: in single quotes, each
// single quote closed, escaped and reopened.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
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
    // Absolute, or under the account's home (~/), which the remote shell
    // expands as it expands the pattern.
    let rest = path.strip_prefix("~/").unwrap_or(path);
    (path.starts_with('/') || path.starts_with("~/"))
        && rest.chars().all(|c| {
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
    // Where an ssh connector's path is; None for one that writes its
    // `script` inline (and for script connectors, which always run here).
    pub fn ssh_location(&self) -> Option<Location> {
        match self.kind {
            ConnectorKind::Script => None,
            ConnectorKind::Ssh if self.script.is_some() => None,
            ConnectorKind::Ssh => self
                .location
                .or(self.project.as_ref().map(|_| Location::Instance)),
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
        match self.output_format.as_deref() {
            None => {}
            Some(_) if self.kind == ConnectorKind::Ssh => errors.push(format!(
                "Connector '{id}' sets output_format, which an ssh connector does not take \
                 — it builds its dataset itself"
            )),
            Some("ansible") => {}
            Some("native") => errors.push(format!(
                "Connector '{id}' sets output_format: native, which 0.36 no longer reads: \
                 change its script to print Ansible inventory JSON (host variables under \
                 _meta.hostvars, each group a top-level key), then remove the key"
            )),
            Some(other) => errors.push(format!(
                "Connector '{id}' sets output_format: {other} — a script prints Ansible \
                 inventory JSON, the one format there is (output_format: ansible says so, \
                 and can be left out)"
            )),
        }
        // Before any early return: an inline script is ssh too.
        if self.kind == ConnectorKind::Ssh && !self.host_args.is_empty() {
            errors.push(format!(
                "Connector '{id}' sets host_args but is not a script connector"
            ));
        }
        if let Some(script) = &self.script {
            if self.kind != ConnectorKind::Ssh {
                errors.push(format!(
                    "Connector '{id}' sets script, which only an ssh connector takes — a \
                     script connector runs a file: set path"
                ));
            }
            if !self.path.is_empty() {
                errors.push(format!(
                    "Connector '{id}' sets both path and script — one or the other: a file, \
                     or the script written here"
                ));
            }
            if self.project.is_some() || self.location.is_some() {
                errors.push(format!(
                    "Connector '{id}' sets script with project or location — a script \
                     written here is in no checkout and on no host"
                ));
            }
            if script.trim().is_empty() {
                errors.push(format!("Connector '{id}' has an empty script"));
            }
            return errors;
        }
        if self.path.trim().is_empty() {
            errors.push(match self.kind {
                ConnectorKind::Ssh => {
                    format!("Connector '{id}' needs a path (a file) or a script (written here)")
                }
                ConnectorKind::Script => format!("Connector '{id}' has an empty path"),
            });
            return errors;
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
                        // A file: one line, no spaces (a command with its
                        // arguments is not a file name), relative only to
                        // a project's checkout.
                        if self.path.contains(char::is_whitespace) {
                            errors.push(format!(
                                "Connector '{id}' has a path with spaces or lines in it — a \
                                 path names a file: put a command (with its arguments) in \
                                 script: instead"
                            ));
                        } else if self.project.is_none()
                            && !std::path::Path::new(&self.path).is_absolute()
                        {
                            errors.push(format!(
                                "Connector '{id}' has a relative path but no project — name \
                                 the project whose checkout holds the script, or use an \
                                 absolute path"
                            ));
                        } else if is_pattern(&self.path) {
                            errors.push(format!(
                                "Connector '{id}' has location: instance but its path is a \
                                 pattern — a pattern reads files on each host: set location: host"
                            ));
                        }
                    }
                    // Neither a location, a project nor a script: what 0.35
                    // wrote, a command or a label in path with each source's
                    // gather_mode saying what it meant. Refused with what to
                    // write instead.
                    None => errors.push(format!(
                        "Connector '{id}' is in the 0.35 form (no location, project or \
                         script), which this version does not read. Write it as 0.36 \
                         reads it: for Ansible facts, location: host with path: \
                         /etc/ansible/facts.d/*.fact; for a command, script: with the \
                         command and its arguments; for a script file, project: (or \
                         location: instance) with its path"
                    )),
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
    fn a_script_connector_defaults_to_the_shared_timeout() {
        let c = connector("name: d42\nproject: prj\npath: d42.py\n");
        assert_eq!(c.kind, ConnectorKind::Script);
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
        let c = connector("name: s\ntype: ssh\npath: gather_facts\nhost_args: [\"{target}\"]\n");
        let errors = c.errors("s").join("\n");
        assert!(errors.contains("host_args"), "{errors}");
        let c = connector("name: s\ntype: ssh\nscript: uname\nhost_args: [\"{target}\"]\n");
        assert!(
            c.errors("s").join("\n").contains("host_args"),
            "inline is ssh too"
        );
        let c = connector("name: s\ntype: ssh\nscript: uname\noutput_format: ansible\n");
        assert!(
            c.errors("s")
                .join("\n")
                .contains("ssh connector does not take")
        );
    }

    #[test]
    fn output_format_ansible_is_accepted_and_the_rest_refused_by_name() {
        let errors = |yaml: &str| connector(yaml).errors("d").join("\n");
        let e = errors("name: d\nproject: prj\npath: d.py\noutput_format: ansible\n");
        assert!(e.is_empty(), "ansible is what every script prints: {e}");
        let e = errors("name: d\nproject: prj\npath: d.py\noutput_format: yaml\n");
        assert!(e.contains("output_format: yaml"), "{e}");
        let e = errors("name: d\nproject: prj\npath: d.py\noutput_format: native\n");
        assert!(e.contains("change its script to print Ansible"), "{e}");
    }

    #[test]
    fn an_ssh_path_is_on_the_hosts_or_a_script_here() {
        let facts =
            connector("name: f\ntype: ssh\nlocation: host\npath: /etc/ansible/facts.d/*.fact\n");
        assert_eq!(facts.ssh_location(), Some(Location::Host));
        assert!(facts.errors("f").is_empty(), "{:?}", facts.errors("f"));

        // A project means a script in its checkout, location: instance.
        let script = connector("name: s\ntype: ssh\nproject: p\npath: ssh/osinfo.sh\n");
        assert_eq!(script.ssh_location(), Some(Location::Instance));
        assert!(script.errors("s").is_empty(), "{:?}", script.errors("s"));

        // Written here: no location, streamed like a file.
        let inline = connector("name: i\ntype: ssh\nscript: |\n  uname -r\n  hostname\n");
        assert_eq!(inline.ssh_location(), None);
        assert!(inline.errors("i").is_empty(), "{:?}", inline.errors("i"));

        // A file on this instance's disk, without a project: said.
        let local =
            connector("name: l\ntype: ssh\nlocation: instance\npath: /opt/scripts/osinfo.sh\n");
        assert_eq!(local.ssh_location(), Some(Location::Instance));
        assert!(local.errors("l").is_empty());
    }

    #[test]
    fn a_home_relative_host_path_is_a_path_the_hosts_may_have() {
        let c = connector("name: f\ntype: ssh\nlocation: host\npath: \"~/facts.d/*.fact\"\n");
        assert!(c.errors("f").is_empty(), "{:?}", c.errors("f"));
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
    fn the_0_35_form_is_refused_saying_what_to_write() {
        // Neither location, project nor script, whatever the path: 0.35's.
        for yaml in [
            "name: o\ntype: ssh\npath: gather_facts\n",
            "name: o\ntype: ssh\npath: \"uname -r\"\n",
            "name: o\ntype: ssh\npath: /etc/ansible/facts.d\n",
        ] {
            let errors = connector(yaml).errors("o").join("");
            assert!(errors.contains("0.35 form"), "{errors}");
            assert!(errors.contains("script:"), "{errors}");
            assert!(errors.contains("location: host"), "{errors}");
        }
    }

    #[test]
    fn a_script_is_written_here_alone() {
        let both = connector("name: s\ntype: ssh\npath: /opt/a.sh\nscript: uname\n");
        assert!(both.errors("s").join("").contains("both path and script"));
        let placed = connector("name: s\ntype: ssh\nproject: p\nscript: uname\n");
        assert!(placed.errors("s").join("").contains("in no checkout"));
        let scripted = connector("name: s\ntype: script\nscript: uname\n");
        assert!(
            scripted
                .errors("s")
                .join("")
                .contains("only an ssh connector")
        );
        let none = connector("name: s\ntype: ssh\n");
        assert!(
            none.errors("s")
                .join("")
                .contains("a path (a file) or a script")
        );
        let command =
            connector("name: s\ntype: ssh\nlocation: instance\npath: \"/opt/report --json\"\n");
        assert!(command.errors("s").join("").contains("spaces or lines"));
    }

    #[test]
    fn a_script_here_is_a_file_not_lines_or_a_pattern() {
        let lines =
            connector("name: s\ntype: ssh\nlocation: instance\npath: \"uname -r\\nhostname\"\n");
        assert!(lines.errors("s").join("").contains("spaces or lines"));
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
