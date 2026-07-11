// Connector adapters (driven): fetch data from a source. One module per
// transport — `process` shells out to a script, `ssh` runs over native SSH,
// `static_inventory` reads Ansible YAML inventory files from disk.
pub mod process;
pub mod ssh;
pub mod static_inventory;
