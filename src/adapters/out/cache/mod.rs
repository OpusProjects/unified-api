// Cache adapters (driven): the in-memory store. `memory` backs CachePort with
// a DashMap; `persistence` optionally snapshots it to disk and reloads at boot.
pub mod memory;
pub mod persistence;
