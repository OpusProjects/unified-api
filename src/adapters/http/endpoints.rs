use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

use crate::AppState;
use crate::domain::dataset::Dataset;

#[derive(Serialize, ToSchema)]
pub struct EndpointInfo {
    pub endpoint_id: String,
    pub name: String,
    pub source_ids: Vec<String>,
    pub sources_ready: usize,
    pub sources_missing: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/endpoints",
    tag = "Endpoints",
    responses(
        (status = 200, description = "List configured endpoints", body = Vec<EndpointInfo>)
    )
)]
pub async fn list_endpoints(State(state): State<Arc<AppState>>) -> Json<Vec<EndpointInfo>> {
    let mut endpoints: Vec<EndpointInfo> = state
        .endpoints
        .iter()
        .map(|(id, ep)| {
            let sources_missing: Vec<String> = ep
                .source_ids
                .iter()
                .filter(|sid| state.cache.get(sid).is_none())
                .cloned()
                .collect();

            let sources_ready = ep.source_ids.len() - sources_missing.len();

            EndpointInfo {
                endpoint_id: id.clone(),
                name: ep.name.clone(),
                source_ids: ep.source_ids.clone(),
                sources_ready,
                sources_missing,
            }
        })
        .collect();

    endpoints.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    Json(endpoints)
}

#[utoipa::path(
    post,
    path = "/api/v1/endpoints/{id}",
    tag = "Endpoints",
    params(
        ("id" = String, Path, description = "Endpoint identifier (e.g. ep-ansible-linux)")
    ),
    request_body(content = Object, description = "Dynamic parameters for the endpoint script (optional)"),
    responses(
        (status = 200, description = "Transformed output from the endpoint script"),
        (status = 404, description = "Endpoint not configured"),
        (status = 503, description = "Required sources not yet synced")
    )
)]
pub async fn run_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Response, StatusCode> {
    let endpoint = state.endpoints.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let params = body.map(|Json(v)| v).unwrap_or(serde_json::json!({}));

    // Collect datasets from configured sources
    let mut datasets: HashMap<String, Dataset> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();

    for source_id in &endpoint.source_ids {
        match state.cache.get(source_id) {
            Some(entry) => {
                datasets.insert(source_id.clone(), entry.dataset.clone());
            }
            None => {
                missing.push(source_id.clone());
            }
        }
    }

    if !missing.is_empty() {
        let body = serde_json::json!({
            "error": "Sources not yet synced",
            "missing_sources": missing
        });
        return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

    let start = Instant::now();

    let result = state
        .output
        .execute(&endpoint.script_path, &endpoint.config, &params, &datasets)
        .await;

    let _duration_ms = start.elapsed().as_millis();

    match result {
        Ok(output) => {
            // The script decides the format — we return the string as-is.
            // We try to detect if it's JSON to set the correct content-type.
            if output.trim_start().starts_with('{') || output.trim_start().starts_with('[') {
                Ok((
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    output,
                )
                    .into_response())
            } else {
                Ok((StatusCode::OK, [("content-type", "text/plain")], output).into_response())
            }
        }
        Err(e) => {
            let body = serde_json::json!({
                "error": e.message
            });
            Ok((StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response())
        }
    }
}
