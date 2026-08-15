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
    let cfg = match unified_api::config::load_config(&config_dir) {
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
        views = cfg.views.len(),
        credentials = cfg.credentials.len(),
        enrichers = cfg.enrichers.len(),
        endpoints = cfg.endpoints.len(),
        projects = cfg.projects.len(),
        api_keys = api_keys.len(),
        "Configuration loaded"
    );

    // The secrets chain, innermost out: env/file resolution, optionally
    // fronted by Vault (credentials with a vault_path read there, the rest
    // fall through), optionally behind the short resolution cache — which
    // stops being a nicety and becomes load-bearing the moment Vault turns
    // every resolution into a network call.
    let secrets: std::sync::Arc<dyn unified_api::ports::secrets::SecretsPort> = {
        let env = EnvSecrets::new(cfg.credentials.clone());
        let base: Box<dyn unified_api::ports::secrets::SecretsPort> =
            match &cfg.secrets_config.vault {
                Some(vault) => Box::new(
                    unified_api::adapters::out::secrets::vault::VaultSecrets::new(
                        vault.clone(),
                        cfg.credentials.clone(),
                        Box::new(env),
                    ),
                ),
                None => Box::new(env),
            };
        match cfg.secrets_config.cache_ttl_seconds {
            0 => std::sync::Arc::from(base),
            ttl => std::sync::Arc::new(
                unified_api::adapters::out::secrets::cache::CachedSecrets::new(
                    base,
                    std::time::Duration::from_secs(ttl),
                ),
            ),
        }
    };

    // Created before the AppState exists because the boot clones below already
    // record into it; handed to the builder so the HTTP layer reads the same
    // instance those clones and the periodic project task write.
    let project_health =
        std::sync::Arc::new(unified_api::domain::sync_health::SyncHealthRegistry::new());

    let (app, state) = unified_api::AppBuilder::new()
        .sources(cfg.sources)
        .views(cfg.views)
        .enrichers(cfg.enrichers)
        .endpoints(cfg.endpoints)
        .projects(
            cfg.projects.clone(),
            std::path::PathBuf::from(&cfg.projects_config.dir),
        )
        .secrets(std::sync::Arc::clone(&secrets))
        .project_health(std::sync::Arc::clone(&project_health))
        .api_keys(api_keys)
        .cors_allowed_origins(cfg.server.cors_allowed_origins)
        .readyz_require_all_sources(cfg.server.readyz_require_all_sources)
        .metrics_require_auth(cfg.server.metrics_require_auth)
        .on_demand_refresh(
            cfg.server.refresh_timeout_seconds,
            cfg.server.refresh_max_concurrent,
        )
        .build_with_state();

    // One switch for every background task. Flipped after the HTTP server has
    // drained; each task returns at its next wait point (a running sync is
    // finished, never cut mid-write) and main joins the handles below before
    // the final snapshot — so the snapshot cannot serialize a cache a sync
    // task is still mutating, and the periodic snapshot task cannot race the
    // final save on the same temp file.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // The handles to join at shutdown. Shared with the background boot task,
    // which is what starts the schedulers (after the clones) and therefore is
    // the one holding their handles.
    let background_tasks: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        std::sync::Arc::default();

    // With persistence configured, reload the last snapshot BEFORE the
    // schedulers start: /readyz is green from second zero and consumers get
    // the pre-restart data while the first syncs run. Then keep snapshotting
    // on an interval.
    if let Some(persistence) = &cfg.cache.persistence {
        let path = std::path::PathBuf::from(&persistence.path);
        unified_api::adapters::out::cache::persistence::load_or_warn(&*state.cache, &path).await;
        let handle = unified_api::adapters::out::cache::persistence::start_snapshot_task(
            std::sync::Arc::clone(&state.cache),
            std::sync::Arc::clone(&state.snapshot_health),
            path,
            persistence.interval_seconds,
            shutdown_rx.clone(),
        );
        background_tasks
            .lock()
            .expect("handle registry")
            .push(handle);
    }

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!(addr = %addr, "Failed to bind: {}", e);
            std::process::exit(1);
        });

    info!(addr = %addr, "Listening");

    // The rest of the boot happens BEHIND the listener: project checkouts and
    // the sync schedulers must never gate /healthz. One unreachable git remote
    // used to mean the listener never bound at all — a failed startup probe
    // for a service whose HTTP layer was perfectly able to serve. Script paths
    // resolve per execution (application::scripts), so nothing here depends on
    // the checkouts existing before the router does; /readyz stays red until
    // the first sync lands, exactly as before.
    {
        let state = std::sync::Arc::clone(&state);
        let secrets = std::sync::Arc::clone(&secrets);
        let project_health = std::sync::Arc::clone(&project_health);
        let projects = cfg.projects.clone();
        let projects_dir = std::path::PathBuf::from(&cfg.projects_config.dir);
        let shutdown_rx = shutdown_rx.clone();
        let mut boot_shutdown = shutdown_rx.clone();
        let tasks = std::sync::Arc::clone(&background_tasks);

        let boot_handle = tokio::spawn(async move {
            if !projects.is_empty() {
                let git: std::sync::Arc<dyn unified_api::ports::git::GitPort> =
                    std::sync::Arc::new(CliGit::new());

                // Concurrently rather than one after the other: boot waits for
                // the slowest clone, not the sum — and every clone is bounded
                // by its project's timeout_seconds (applied in sync_project).
                let mut clones = tokio::task::JoinSet::new();
                for (project_id, project) in projects.clone() {
                    // sync_on_boot=false + existing checkout (e.g. a persistent
                    // volume) = start offline from what is on disk; updates then
                    // come from the interval or POST /api/v1/projects/{id}/sync.
                    // A missing checkout is always cloned — no scripts, nothing
                    // to run.
                    let checkout_exists =
                        tokio::fs::try_exists(projects_dir.join(&project_id).join(".git"))
                            .await
                            .unwrap_or(false);
                    if !project.sync_on_boot && checkout_exists {
                        info!(project = %project_id, "Using existing checkout (sync_on_boot: false)");
                        continue;
                    }

                    let git = std::sync::Arc::clone(&git);
                    let secrets = std::sync::Arc::clone(&secrets);
                    let project_health = std::sync::Arc::clone(&project_health);
                    let projects_dir = projects_dir.clone();
                    clones.spawn(async move {
                        match unified_api::application::projects::sync_project(
                            &*git,
                            &*secrets,
                            &project_health,
                            &project_id,
                            &project,
                            &projects_dir,
                        )
                        .await
                        {
                            Ok(()) => info!(project = %project_id, "Project checkout ready"),
                            Err(e) => {
                                error!(project = %project_id, error = %e, "Project sync failed")
                            }
                        }
                    });
                }
                // A shutdown arriving mid-clone aborts the remaining clones
                // (dropping them kills the git children) instead of starting
                // schedulers nobody wants anymore.
                loop {
                    tokio::select! {
                        next = clones.join_next() => {
                            if next.is_none() {
                                break;
                            }
                        }
                        _ = boot_shutdown.changed() => {
                            clones.abort_all();
                            return;
                        }
                    }
                }

                let project_tasks =
                    unified_api::adapters::r#in::scheduler::start_project_sync_tasks(
                        git,
                        secrets,
                        project_health,
                        projects,
                        projects_dir,
                        shutdown_rx.clone(),
                    );
                tasks.lock().expect("handle registry").extend(project_tasks);
            }

            if *boot_shutdown.borrow() {
                return;
            }
            // After the boot clones had their chance (bounded by their
            // timeouts), so a source's first sync does not race its own
            // script's clone and fail for no reason but start order.
            let sync_tasks =
                unified_api::adapters::r#in::scheduler::start_sync_tasks(state, shutdown_rx);
            tasks.lock().expect("handle registry").extend(sync_tasks);
        });
        background_tasks
            .lock()
            .expect("handle registry")
            .push(boot_handle);
    }

    // Graceful shutdown — waits for SIGTERM or Ctrl+C
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            error!("Server error: {}", e);
            std::process::exit(1);
        });

    // Drain the background tasks before touching the disk: signal them, then
    // wait — bounded by shutdown_grace_seconds — for in-flight runs to finish.
    // Past the grace the snapshot proceeds anyway (best effort, exactly the
    // pre-drain behavior), because blocking exit on a wedged sync would trade
    // a possibly-torn snapshot for a SIGKILL and no snapshot at all.
    let _ = shutdown_tx.send(true);
    let handles: Vec<tokio::task::JoinHandle<()>> =
        std::mem::take(background_tasks.lock().expect("handle registry").as_mut());
    let grace = std::time::Duration::from_secs(cfg.server.shutdown_grace_seconds);
    let drain = async {
        for handle in handles {
            let _ = handle.await;
        }
    };
    if tokio::time::timeout(grace, drain).await.is_err() {
        warn!(
            grace_seconds = cfg.server.shutdown_grace_seconds,
            "Background tasks still running after the grace period — snapshotting anyway"
        );
    } else {
        info!("Background tasks drained");
    }

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
