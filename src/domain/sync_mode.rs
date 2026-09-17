use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// How data is applied to cache when it arrives
#[derive(Debug, Deserialize, Serialize, ToSchema, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    // Clears everything and puts new data — the script brings the complete inventory
    #[default]
    Replace,
    // Patches only what comes — the rest is left alone
    Merge,
}
