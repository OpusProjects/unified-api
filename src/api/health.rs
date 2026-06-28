use axum::extract::State;
use std::sync::Arc;

use crate::AppState;

// State<Arc<AppState>> es cómo Axum inyecta el estado compartido en un handler.
// Axum ve el parámetro y automáticamente le pasa el state que configuramos
// con .with_state() — no lo llamamos nosotros, Axum lo hace.
// Es inyección de dependencias, como en Spring o FastAPI.

pub async fn healthz(State(_state): State<Arc<AppState>>) -> &'static str {
    "ok"
}

pub async fn readyz(State(state): State<Arc<AppState>>) -> &'static str {
    // Ejemplo: el cache es accesible via state.cache
    // En el futuro comprobaremos que todas las sources
    // se han sincronizado al menos una vez
    let _keys = state.cache.keys();
    "ok"
}
