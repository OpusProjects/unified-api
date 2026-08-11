// Connector adapters (driven): fetch data from a source. One module per
// transport — `process` shells out to a script, `ssh` runs over native SSH,
// `static_inventory` reads Ansible YAML inventory files from disk,
// `remote` federates another unified-api instance over HTTP.
pub mod process;
pub mod remote;
pub mod ssh;
// OpenSSH known_hosts parsing/matching for the SSH connector's host key
// verification — kept out of ssh.rs so the format rules are testable alone.
pub(crate) mod known_hosts;
pub mod static_inventory;
