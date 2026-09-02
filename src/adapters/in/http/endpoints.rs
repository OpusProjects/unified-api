use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::domain::dataset::Dataset;

// The 503's wire shape: ErrorBody plus the sources still missing, so the
// caller knows what to wait for instead of polling blind.
#[derive(Serialize, ToSchema)]
pub struct EndpointUnavailableBody {
    /// Human-readable explanation, same contract as ErrorBody's field.
    pub error: String,
    /// The configured sources that have no cache entry yet.
    pub missing_sources: Vec<String>,
}

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
pub async fn list_endpoints(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<Vec<EndpointInfo>> {
    let config = state.config();
    let mut endpoints: Vec<EndpointInfo> = config
        .endpoints
        .iter()
        .filter(|(id, _)| auth.permissions.allows_endpoint(id))
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
        (status = 403, description = "API key not allowed to run this endpoint", body = ErrorBody),
        (status = 404, description = "Endpoint not configured", body = ErrorBody),
        (status = 500, description = "The transformer failed; the body carries its error", body = ErrorBody),
        (status = 503, description = "Required sources not yet synced — the body lists them", body = EndpointUnavailableBody),
        (status = 504, description = "A script transformer exceeded timeout_seconds and was killed", body = ErrorBody)
    )
)]
pub async fn run_endpoint(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Result<Response, ApiError> {
    let params = body.map(|Json(v)| v).unwrap_or(serde_json::json!({}));
    execute_endpoint(&state, &auth, id, params, &headers).await
}

#[utoipa::path(
    get,
    path = "/api/v1/endpoints/{id}",
    tag = "Endpoints",
    params(
        ("id" = String, Path, description = "Endpoint identifier (e.g. ep-ansible-linux)")
    ),
    responses(
        (status = 200, description = "Transformed output from the endpoint script. Query parameters become the endpoint's dynamic parameters, all as strings"),
        (status = 403, description = "API key not allowed to run this endpoint", body = ErrorBody),
        (status = 404, description = "Endpoint not configured", body = ErrorBody),
        (status = 500, description = "The transformer failed; the body carries its error", body = ErrorBody),
        (status = 503, description = "Required sources not yet synced — the body lists them", body = EndpointUnavailableBody),
        (status = 504, description = "A script transformer exceeded timeout_seconds and was killed", body = ErrorBody)
    )
)]
pub async fn run_endpoint_get(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // Rendering an inventory is a read, so it should be reachable with GET:
    // browsers, proxy caches and tools that only fetch URLs could not call
    // the POST-only route at all.
    //
    // A query string has no types, so every parameter arrives as a string.
    // The script receives the same ENDPOINT_PARAMS object either way, so a
    // transformer that already coerces its inputs needs no change; one that
    // needs real numbers, booleans or nesting still wants POST.
    let params = serde_json::Value::Object(
        query
            .into_iter()
            .map(|(key, value)| (key, serde_json::Value::String(value)))
            .collect(),
    );
    execute_endpoint(&state, &auth, id, params, &headers).await
}

// The shared body of both methods: authorize, collect the datasets, run the
// transformer under its timeout, record metrics.
async fn execute_endpoint(
    state: &Arc<AppState>,
    auth: &AuthContext,
    id: String,
    params: serde_json::Value,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    // Granting an endpoint implicitly grants reading its output, even when
    // the key cannot read the underlying sources directly — the endpoint IS
    // the product being granted (e.g. a rendered inventory).
    if !auth.permissions.allows_endpoint(&id) {
        return Err(ApiError::forbidden(format!(
            "this API key is not allowed to run endpoint '{}'",
            id
        )));
    }
    let config = state.config();
    let endpoint = config
        .endpoints
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("endpoint '{}' is not configured", id)))?;

    // Collect datasets from configured sources (Arc clones — shared with the
    // cache, not deep copies)
    let mut datasets: HashMap<String, Arc<Dataset>> = HashMap::new();
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
        // The one failure whose body carries more than the message: naming
        // the sources still missing is what tells the caller what to wait
        // for. A typed struct rather than an ad-hoc json! so the OpenAPI
        // spec can declare the shape.
        missing.sort();
        let body = EndpointUnavailableBody {
            error: "Sources not yet synced".to_string(),
            missing_sources: missing,
        };
        return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

    // A constructed inventory: merge everything, return only the part the
    // limit describes. It happens here, before the transformer is chosen, so
    // a builtin and a script see exactly the same scope.
    let datasets = match &endpoint.limit {
        Some(limit) => crate::application::output::apply_limit(&datasets, limit),
        None => datasets,
    };

    let start = Instant::now();

    // A builtin's format is known, so its content type is too; a script's
    // output is sniffed below instead.
    let builtin_content_type = endpoint.output.map(|format| match format {
        crate::domain::endpoint::OutputFormat::Ansible
        | crate::domain::endpoint::OutputFormat::Json => "application/json",
        crate::domain::endpoint::OutputFormat::Csv => "text/csv",
    });

    let result = match endpoint.output {
        // Builtin transformer: in-process, no script. The render is pure CPU
        // over the whole inventory, so it runs on the blocking pool — an async
        // worker stalled for the length of a big merge would stall every other
        // request scheduled on it too.
        Some(format) => {
            let config = endpoint.config.clone();
            let rendered = tokio::task::spawn_blocking(move || match format {
                crate::domain::endpoint::OutputFormat::Ansible => {
                    crate::application::output::render_ansible(&datasets, &config, &params)
                }
                crate::domain::endpoint::OutputFormat::Json => {
                    crate::application::output::render_json(&datasets, &config, &params)
                }
                crate::domain::endpoint::OutputFormat::Csv => {
                    crate::application::output::render_csv(&datasets, &config, &params)
                }
            })
            .await;
            // Only a panic in the render lands in the error arm; it flows
            // through the metrics below like any other failed run rather
            // than returning early.
            rendered.map_err(|join_error| {
                ApiError::internal(format!("builtin transformer failed: {}", join_error))
            })
        }
        // Script transformer: resolve the path (+ venv) and run it under its timeout.
        None => {
            let script_path = match endpoint.script_path.as_deref() {
                Some(path) => path,
                None => {
                    // Config validation guarantees exactly one of output /
                    // script_path; handled rather than panicking if it slips through.
                    return Err(ApiError::internal(format!(
                        "endpoint '{}' has neither output nor script_path",
                        id
                    )));
                }
            };

            // An endpoint that names a project runs its transformer from the
            // checkout; resolved per execution like sources and enrichers.
            let script_path = match &endpoint.project_id {
                Some(project_id) => crate::application::scripts::resolve_script_path(
                    &state.projects_dir,
                    &id,
                    project_id,
                    script_path,
                ),
                None => script_path.to_string(),
            };

            // The project's virtualenv rides the same reserved-config channel as
            // for connectors and enrichers; the process adapter prepends it to PATH.
            let mut config = endpoint.config.clone();
            if let Some(project_id) = &endpoint.project_id
                && let Some(bin) =
                    crate::application::scripts::venv_bin_dir(&state.projects_dir, project_id)
            {
                config.insert(crate::ports::venv::VENV_BIN_CONFIG_KEY.to_string(), bin);
            }
            // Who asked, for the transformer's own logs — the request id inside
            // ENDPOINT_CONFIG as `trigger`.
            if let Some(request_id) = headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
            {
                config.insert("trigger".to_string(), request_id.to_string());
            }

            // A hung transformer must not hang the HTTP request forever.
            let timeout_seconds = endpoint
                .timeout_seconds
                .unwrap_or_else(crate::domain::default_timeout_seconds);
            match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_seconds),
                state.output.execute(
                    &script_path,
                    &endpoint.script_args,
                    &config,
                    &params,
                    &datasets,
                ),
            )
            .await
            {
                // The script's own failure and the timeout both flow into the
                // shared result: a timed-out run used to return before the
                // metrics below, so `unified_api_endpoint_total` never
                // counted it — despite being exactly the run alerting cares
                // about most.
                Ok(result) => result.map_err(|e| ApiError::internal(e.message)),
                Err(_elapsed) => Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("endpoint timed out after {}s", timeout_seconds),
                )),
            }
        }
    };

    let duration_ms = start.elapsed().as_millis();

    let result_label = if result.is_ok() { "success" } else { "error" };
    metrics::counter!(
        "unified_api_endpoint_total",
        "endpoint" => id.clone(),
        "result" => result_label,
    )
    .increment(1);
    metrics::histogram!(
        "unified_api_endpoint_duration_seconds",
        "endpoint" => id.clone(),
    )
    .record(duration_ms as f64 / 1000.0);

    // A failed run renders through ApiError like every other failure in the
    // API — the counters above have already recorded it.
    let output = result?;

    // A builtin's content type is known from its format; a script decides its
    // own format, so its output is sniffed for JSON.
    if let Some(content_type) = builtin_content_type {
        return Ok((StatusCode::OK, [("content-type", content_type)], output).into_response());
    }
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
