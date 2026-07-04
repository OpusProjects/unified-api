use serde::Deserialize;

// How data is applied to cache when it arrives
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    // Clears everything and puts new data — the script brings the complete inventory
    #[default]
    Replace,
    // Patches only what comes — the rest is left alone
    Merge,
}
