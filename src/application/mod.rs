// The application layer: the USE CASES of the service.
//
// Orchestrates domain + ports and knows nothing about HTTP or timers. Both
// HTTP handlers (api/) and the scheduler call here, so the logic of
// each use case exists in ONE place only — previously sync was duplicated
// between the handler and the scheduler, and the two copies had already diverged.
pub mod credentials;
pub mod enrich;
pub mod sync;
