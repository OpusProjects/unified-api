use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use unified_api::adapters::out::git::cli::CliGit;
use unified_api::adapters::out::secrets::env::EnvSecrets;

#[tokio::main]
async fn main() {
    // Structured logging — level configurable with RUST_LOG env var
    // RUST_LOG=debug cargo run → shows debug+info+warn+error
    // RUST_LOG=unified_api=debug → only debug from our crate
    // Without RUST_LOG → default is info
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_dir = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "config".to_string());
    let mut cfg = match unified_api::config::load_config(&config_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Secrets are read here, at the boundary: the rest of the app receives
    // resolved keys as parameters and does not touch environment variables.
    let api_keys = match resolve_api_keys(&cfg) {
        Ok(keys) => keys,
        Err(e) => {
            error!("Failed to resolve API keys: {}", e);
            std::process::exit(1);
        }
    };

    // Make an unauthenticated deployment loud, not a buried auth=false field
    if api_keys.is_empty() {
        warn!(
            "No API keys configured (api_keys.yaml or UNIFIED_API_KEY): \
             the /api/v1 API is running WITHOUT authentication"
        );
    }

    info!(
        sources = cfg.sources.len(),
        credentials = cfg.credentials.len(),
        enrichers = cfg.enrichers.len(),
        endpoints = cfg.endpoints.len(),
        projects = cfg.projects.len(),
        api_keys = api_keys.len(),
        "Configuration loaded"
    );

    let secrets: std::sync::Arc<dyn unified_api::ports::secrets::SecretsPort> =
        std::sync::Arc::new(EnvSecrets::new(cfg.credentials.clone()));

    // Bring project checkouts up to date BEFORE building the app, so script
    // paths can be resolved into them. A failed clone logs an error and the
    // boot continues: the affected source fails loudly at sync time and the
    // periodic project task (if configured) retries.
    if !cfg.projects.is_empty() {
        let git: std::sync::Arc<dyn unified_api::ports::git::GitPort> =
            std::sync::Arc::new(CliGit::new());
        let projects_dir = std::path::PathBuf::from(&cfg.projects_config.dir);

        for (project_id, project) in &cfg.projects {
            // sync_on_boot=false + existing checkout (e.g. a persistent
            // volume) = start offline from what is on disk; updates then come
            // from the interval or POST /api/v1/projects/{id}/sync. A missing
            // checkout is always cloned — no scripts, nothing to run.
            let checkout_exists = projects_dir.join(project_id).join(".git").exists();
            if !project.sync_on_boot && checkout_exists {
                info!(project = %project_id, "Using existing checkout (sync_on_boot: false)");
                continue;
            }

            match unified_api::application::projects::sync_project(
                &*git,
                &*secrets,
                project_id,
                project,
                &projects_dir,
            )
            .await
            {
                Ok(()) => info!(project = %project_id, "Project checkout ready"),
                Err(e) => error!(project = %project_id, error = %e, "Project sync failed"),
            }
        }

        cfg.resolve_script_paths(&projects_dir);

        unified_api::adapters::r#in::scheduler::start_project_sync_tasks(
            git,
            std::sync::Arc::clone(&secrets),
            cfg.projects.clone(),
            projects_dir,
        );
    }

    let (app, state) = unified_api::AppBuilder::new()
        .sources(cfg.sources)
        .enrichers(cfg.enrichers)
        .endpoints(cfg.endpoints)
        .projects(
            cfg.projects.clone(),
            std::path::PathBuf::from(&cfg.projects_config.dir),
        )
        .secrets(std::sync::Arc::clone(&secrets))
        .api_keys(api_keys)
        .cors_allowed_origins(cfg.server.cors_allowed_origins)
        .build_with_state();

    // With persistence configured, reload the last snapshot BEFORE the
    // schedulers start: /readyz is green from second zero and consumers get
    // the pre-restart data while the first syncs run. Then keep snapshotting
    // on an interval.
    if let Some(persistence) = &cfg.cache.persistence {
        let path = std::path::PathBuf::from(&persistence.path);
        unified_api::adapters::out::cache::persistence::load_or_warn(&*state.cache, &path).await;
        unified_api::adapters::out::cache::persistence::start_snapshot_task(
            std::sync::Arc::clone(&state.cache),
            path,
            persistence.interval_seconds,
        );
    }

    unified_api::adapters::r#in::scheduler::start_sync_tasks(std::sync::Arc::clone(&state));

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!(addr = %addr, "Failed to bind: {}", e);
            std::process::exit(1);
        });

    info!(addr = %addr, "Listening");

    // Graceful shutdown — waits for SIGTERM or Ctrl+C
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            error!("Server error: {}", e);
            std::process::exit(1);
        });

    // Final snapshot on graceful shutdown, so the file reflects everything up
    // to the last second (the interval task may not have fired recently).
    if let Some(persistence) = &cfg.cache.persistence {
        let path = std::path::Path::new(&persistence.path);
        match unified_api::adapters::out::cache::persistence::save(&*state.cache, path).await {
            Ok(count) => info!(entries = count, "Final cache snapshot saved"),
            Err(e) => error!(error = %e, "Final cache snapshot failed"),
        }
    }

    info!("Shutdown complete");
}

// Turn the api_keys.yaml definitions into runtime keys by reading each
// declared env var. A declared-but-missing env var is a hard startup error:
// the alternative (skip the key with a warn) means a typo silently locks a
// consumer out. The legacy UNIFIED_API_KEY, if set, joins as an admin key —
// existing deployments keep working unchanged.
fn resolve_api_keys(
    cfg: &unified_api::config::AppConfig,
) -> Result<Vec<unified_api::adapters::r#in::http::auth::ResolvedApiKey>, String> {
    use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
    use unified_api::domain::api_key::ApiKeyRole;

    let mut keys = Vec::new();

    // BTreeMap-like deterministic order helps tests and logs
    let mut ids: Vec<&String> = cfg.api_keys.keys().collect();
    ids.sort();

    for id in ids {
        let def = &cfg.api_keys[id];
        let secret = std::env::var(&def.env).map_err(|_| {
            format!(
                "API key '{}' expects the secret in env var '{}', which is not set",
                id, def.env
            )
        })?;
        if secret.is_empty() {
            return Err(format!(
                "API key '{}': env var '{}' is set but empty",
                id, def.env
            ));
        }

        let permissions = match def.role {
            ApiKeyRole::Admin => Permissions::Admin,
            ApiKeyRole::Restricted => Permissions::Scoped {
                sources: def.sources.iter().cloned().collect(),
                endpoints: def.endpoints.iter().cloned().collect(),
            },
        };

        keys.push(ResolvedApiKey {
            name: def.name.clone(),
            secret,
            permissions,
        });
    }

    if let Ok(secret) = std::env::var("UNIFIED_API_KEY")
        && !secret.is_empty()
    {
        keys.push(ResolvedApiKey {
            name: "default".to_string(),
            secret,
            permissions: Permissions::Admin,
        });
    }

    Ok(keys)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down"),
        _ = terminate => info!("Received SIGTERM, shutting down"),
    }
}
