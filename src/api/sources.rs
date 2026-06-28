use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

use crate::AppState;

// Struct para la respuesta JSON de listar sources cacheados
// Serialize para poder convertirlo a JSON automáticamente
#[derive(Serialize)]
pub struct CachedSourceInfo {
    pub source_id: String,
    pub is_fresh: bool,
    pub age_seconds: u64,
    pub total_hosts: usize, // usize = entero sin signo del tamaño del puntero de la plataforma
}

// GET /api/v1/sources — lista todos los sources cacheados con su estado
// Json<Vec<...>> = Axum serializa automáticamente a JSON con Content-Type application/json
pub async fn list_cached_sources(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<CachedSourceInfo>> {
    let keys = state.cache.keys();

    // .iter() + .filter_map() = recorre las keys y filtra las que tienen valor
    // Es como: [info for key in keys if (entry := cache.get(key)) is not None]
    let sources: Vec<CachedSourceInfo> = keys
        .iter()
        .filter_map(|key| {
            let entry = state.cache.get(key)?; // ? en closures: si None, salta este elemento
            Some(CachedSourceInfo {
                source_id: key.clone(),
                is_fresh: entry.is_fresh(),
                age_seconds: entry.age_seconds(),
                total_hosts: entry.dataset.hostvars.len(),
            })
        })
        .collect();

    Json(sources)
}

// GET /api/v1/sources/:id/dataset — devuelve el dataset cacheado de un source
// Path(id) = Axum extrae el :id de la URL automáticamente
// Result<Json, StatusCode> = devuelve JSON si existe, o 404 si no
pub async fn get_source_dataset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.cache.get(&id) {
        Some(entry) => {
            // serde_json::to_value convierte el struct a un JSON genérico
            let json = serde_json::to_value(&entry.dataset)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(json))
        }
        // None = no existe en cache → 404
        None => Err(StatusCode::NOT_FOUND),
    }
}
