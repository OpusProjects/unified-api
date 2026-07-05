// Connector adapters (driven): fetch data from a source. One module per
// transport — `process` shells out to a script, `ssh` runs over native SSH.
pub mod process;
pub mod ssh;
