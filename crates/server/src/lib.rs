// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! HTTP front of the orchestrator.
//!
//! Two token classes, both bearer tokens, both narrowly scoped:
//!
//! - a **worker token** may lease attempts of its pool, download exactly the artifacts the
//!   leased job references, heartbeat, upload artifacts and post the attempt record. It can never
//!   create tasks, read other attempts or publish anything. Assume a full-access job steals it.
//! - the **admin token** creates tasks and revisions, closes tasks and reads task state. It is the
//!   authenticated human input the design requires until a UI exists.
//!
//! Git/forge operations remain serialized. UI reads and worker transport use separate state
//! handles with short database locking, so repository preparation cannot block heartbeats.
//! All synchronous work runs on the blocking pool; uploads are bounded and hash-checked.

mod admin;
mod auth;
pub mod ui;
mod worker;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use artidezi_orchestrator::{Orchestrator, OrchestratorError};
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// Artifact uploads stream to disk and have their own limits; every other route takes JSON.
const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub lease_ttl: Duration,
    pub tick_interval: Duration,
    pub ui: Option<ui::UiConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".parse().expect("valid address"),
            lease_ttl: Duration::from_secs(120),
            tick_interval: Duration::from_secs(30),
            ui: None,
        }
    }
}

/// Bearer tokens, resolved from secrets before the server starts.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub workers: Vec<WorkerToken>,
    pub admin: String,
}

/// A token identifies exactly one worker.
#[derive(Clone, Debug)]
pub struct WorkerToken {
    pub worker_pool: String,
    pub worker_id: String,
    pub token: String,
}

pub struct AppState {
    pub orchestrator: Mutex<Orchestrator>,
    access: artidezi_orchestrator::state::StateAccess,
    operation_gate: Arc<tokio::sync::Semaphore>,
    upload_slots: Arc<tokio::sync::Semaphore>,
    /// Per pool, so that one pool's hostile bundles can't delay another's results.
    inspection_slots: BTreeMap<String, Arc<tokio::sync::Semaphore>>,
    limits: artidezi_config::model::InstanceLimits,
    pub tokens: Tokens,
    pub lease_ttl: Duration,
}

impl AppState {
    pub fn new(orchestrator: Orchestrator, tokens: Tokens, lease_ttl: Duration) -> Self {
        let access = orchestrator.state_access();
        let limits = orchestrator.config.instance.limits.clone();
        let inspection_slots = orchestrator
            .config
            .worker_pools
            .keys()
            .map(|pool| {
                let slots = limits.max_concurrent_inspections as usize;
                (pool.clone(), Arc::new(tokio::sync::Semaphore::new(slots)))
            })
            .collect();
        Self {
            upload_slots: Arc::new(tokio::sync::Semaphore::new(
                limits.max_concurrent_uploads as usize,
            )),
            inspection_slots,
            limits,
            orchestrator: Mutex::new(orchestrator),
            access,
            operation_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            tokens,
            lease_ttl,
        }
    }
}

pub type SharedState = Arc<AppState>;

/// API error with an HTTP status; every handler returns this so failures are JSON.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden")
    }
}

impl From<OrchestratorError> for ApiError {
    fn from(error: OrchestratorError) -> Self {
        use artidezi_store::StoreError;
        match &error {
            OrchestratorError::Store(StoreError::NotFound { .. }) => {
                Self::not_found(error.to_string())
            }
            OrchestratorError::Store(StoreError::InvalidTransition { .. }) => {
                Self::new(StatusCode::CONFLICT, error.to_string())
            }
            OrchestratorError::Invalid(_)
            | OrchestratorError::Resolve(_)
            | OrchestratorError::SourceHeadChanged { .. } => Self::bad_request(error.to_string()),
            _ => Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }
}

impl From<artidezi_store::StoreError> for ApiError {
    fn from(error: artidezi_store::StoreError) -> Self {
        OrchestratorError::from(error).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(status = %self.status, "{}", self.message);
        }
        let body = artidezi_contract::wire::ErrorResponse {
            error: self.message,
        };
        (self.status, axum::Json(body)).into_response()
    }
}

/// Runs a closure against the orchestrator on the blocking pool.
pub async fn with_orchestrator<T, F>(state: &SharedState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut Orchestrator) -> Result<T, ApiError> + Send + 'static,
{
    // Wait asynchronously: queued operations must not consume the blocking threads needed by
    // state reads/heartbeats. The permit stays with the operation even if its HTTP caller leaves.
    let permit = state
        .operation_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "operation queue closed"))?;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut guard = state
            .orchestrator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?
}

/// Reads local state or serves worker transport without waiting for Git/forge operations.
/// Each database method is serialized internally; artifact reads/writes hold no database lock.
pub async fn with_state<T, F>(state: &SharedState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut artidezi_orchestrator::state::StateAccess) -> Result<T, ApiError>
        + Send
        + 'static,
{
    let mut access = state.access.clone();
    tokio::task::spawn_blocking(move || f(&mut access))
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
}

pub fn router(state: SharedState) -> Router {
    let worker = Router::new()
        .route("/worker/v1/lease", post(worker::lease))
        .route("/worker/v1/status", post(worker::status))
        .route(
            "/worker/v1/attempts/{attempt_id}/heartbeat",
            post(worker::heartbeat),
        )
        .route(
            "/worker/v1/attempts/{attempt_id}/artifacts/{sha256}",
            get(worker::download).put(worker::upload),
        )
        .route(
            "/worker/v1/attempts/{attempt_id}/record",
            post(worker::record),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::worker_middleware,
        ));
    let admin = Router::new()
        .route("/admin/v1/workers", get(admin::list_workers))
        .route(
            "/admin/v1/maintenance",
            get(admin::maintenance).post(admin::set_maintenance),
        )
        .route("/admin/v1/workers/drain", post(admin::drain_worker))
        .route(
            "/admin/v1/tasks",
            post(admin::create_task).get(admin::list_tasks),
        )
        .route("/admin/v1/tasks/{task_id}", get(admin::get_task))
        .route(
            "/admin/v1/tasks/{task_id}/revisions",
            post(admin::request_revision),
        )
        .route(
            "/admin/v1/tasks/{task_id}/proposal-revisions",
            post(admin::revise_proposal),
        )
        .route("/admin/v1/tasks/{task_id}/close", post(admin::close_task))
        .route(
            "/admin/v1/tasks/{task_id}/retry-publication",
            post(admin::retry_publication),
        )
        .route(
            "/admin/v1/tasks/{task_id}/authorize-workflow-publication",
            post(admin::authorize_workflow_publication),
        )
        .route(
            "/admin/v1/tasks/{task_id}/reconcile",
            post(admin::reconcile_task),
        )
        .route("/admin/v1/tick", post(admin::tick))
        .route(
            "/admin/v1/artifacts/cleanup",
            post(admin::cleanup_artifacts),
        )
        .route("/admin/v1/discovery", get(admin::list_pull_requests))
        .route(
            "/admin/v1/discovery/{id}/pause",
            post(admin::pause_pull_request),
        )
        .route("/admin/v1/discovery/scan", post(admin::scan_pull_requests))
        .route(
            "/admin/v1/discovery/{id}/resume",
            post(admin::resume_pull_request),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::admin_middleware,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(worker)
        .merge(admin)
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .with_state(state)
}

/// The UI is opt-in. Without a separately supplied UI password, no UI routes exist.
pub fn router_with_ui(state: SharedState, ui: Option<ui::UiConfig>) -> Router {
    let app = router(state.clone());
    match ui {
        Some(config) => app.merge(ui::router(state, config)),
        None => app,
    }
}

/// Binds, serves and runs the maintenance loop until shutdown.
pub async fn serve(
    orchestrator: Orchestrator,
    tokens: Tokens,
    config: ServerConfig,
) -> std::io::Result<()> {
    let state: SharedState = Arc::new(AppState::new(orchestrator, tokens, config.lease_ttl));
    let app = router_with_ui(state.clone(), config.ui);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(listen = %config.listen, "artidezi orchestrator listening");

    let tick_state = state.clone();
    let recovery_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let result = with_state(&recovery_state, |o| {
                Ok(o.recover_expired_at(chrono::Utc::now())?)
            })
            .await;
            match result {
                Ok(lost) if !lost.is_empty() => {
                    tracing::warn!(?lost, "expired worker leases recovered")
                }
                Err(error) => tracing::error!(%error.message, "worker recovery failed"),
                _ => {}
            }
        }
    });
    let tick_interval = config.tick_interval;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick_interval);
        loop {
            interval.tick().await;
            let result = with_orchestrator(&tick_state, |o| Ok(o.tick()?)).await;
            match result {
                Ok(report) => {
                    if !report.lost_attempts.is_empty() || !report.errors.is_empty() {
                        tracing::warn!(?report, "maintenance tick");
                    }
                }
                Err(e) => tracing::error!("maintenance tick failed: {}", e.message),
            }
        }
    });

    // `docker stop` and systemd send SIGTERM, which PID 1 in a container ignores unless handled.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            tracing::info!("shutting down");
        })
        .await
}
