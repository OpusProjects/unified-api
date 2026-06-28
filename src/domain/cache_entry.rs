use std::time::{Duration, Instant};

use super::dataset::Dataset;

// CacheEntry envuelve un Dataset con metadata de caché:
// cuándo se obtuvo y cuánto tiempo es válido.
// No deriva Deserialize porque no viene de un archivo —
// se crea en runtime cuando un source se sincroniza.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub dataset: Dataset,
    pub fetched_at: Instant,  // Instant = marca de tiempo monotónica (no fecha, sino "cuánto ha pasado")
    pub ttl: Duration,        // Duration = duración de tiempo (ej: 3600 segundos)
}

// `impl` = bloque de implementación — aquí definimos los métodos del struct.
// Es como definir métodos dentro de una clase en Python.
impl CacheEntry {
    // `pub fn new(...)` es un "constructor" por convención.
    // No hay keyword especial como __init__ — es simplemente una función
    // que devuelve una instancia del struct.
    pub fn new(dataset: Dataset, ttl_seconds: u64) -> Self {
        // Self = el propio tipo (CacheEntry). Es como `self.__class__` en Python.
        Self {
            dataset,
            fetched_at: Instant::now(),
            ttl: Duration::from_secs(ttl_seconds),
        }
    }

    // &self = referencia inmutable a la instancia (como self en Python,
    // pero el & dice "solo la leo, no la modifico")
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() < self.ttl
    }

    // Cuántos segundos lleva en caché
    pub fn age_seconds(&self) -> u64 {
        self.fetched_at.elapsed().as_secs()
    }
}
