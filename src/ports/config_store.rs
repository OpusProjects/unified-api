use crate::config::AppConfig;

// The configuration directory as something the application can read, propose
// changes to, and commit — rather than as a directory the process happens to
// have read once at startup.
//
// A port, so the use case is written against "a place configuration lives"
// and the filesystem details (staging, atomic renames, which temp file) stay
// in the adapter. The types are deliberately about WHOLE FILES: this mirrors
// the deployment unit a configuration-as-code pipeline already works in — it
// renders `sources.yaml`, not a patch to a source — and it keeps YAML
// comments and key order intact, which any parse-and-reserialize scheme
// would quietly destroy.
//
// The port and its types are web-rs's (the `web` repository), generic over
// what a directory loads into; this service's store loads an AppConfig.
pub use web_rs::config::{ConfigChange, ConfigFileStat, ConfigStore};

pub type ConfigStorePort = dyn ConfigStore<Config = AppConfig>;
