// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Admin routes: the authenticated human input until a UI exists.

use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::TaskSelection;
use artidezi_store::model::*;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::auth::require_admin;
use crate::{ApiError, SharedState, with_orchestrator, with_state};

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub project: String,
    pub instructions: String,
    #[serde(default)]
    pub agent_profile: Option<String>,
    #[serde(default)]
    pub delivery_mode: Option<DeliveryMode>,
    #[serde(default = "default_actor")]
    pub created_by: String,
}

fn default_actor() -> String {
    "admin".to_owned()
}

pub async fn list_workers(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Worker>>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_state(&state, |o| Ok(o.db.list_workers()?)).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrainWorkerRequest {
    pub worker_pool: String,
    pub worker_id: String,
    pub drained: bool,
}

#[derive(Deserialize)]
pub struct MaintenanceRequest {
    pub discovery_paused: bool,
}

pub async fn maintenance(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_state(&state, |o| {
            Ok(serde_json::json!({"discovery_paused":o.db.discovery_paused()?}))
        })
        .await?,
    ))
}

pub async fn set_maintenance(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<MaintenanceRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_admin(&state, &headers)?;
    with_orchestrator(&state, move |o| {
        o.db.set_discovery_paused(request.discovery_paused, "admin")?;
        Ok(())
    })
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub async fn drain_worker(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<DrainWorkerRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_admin(&state, &headers)?;
    with_state(&state, move |o| {
        o.db.drain_worker(
            &request.worker_pool,
            &request.worker_id,
            request.drained,
            "admin",
        )?;
        Ok(())
    })
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct RevisionRequest {
    pub feedback: String,
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Debug, Deserialize)]
pub struct ActorRequest {
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPublicationRequest {
    pub attempt_id: String,
    pub attempt_record_sha256: String,
    pub reason: String,
    pub actor: String,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub state: Option<TaskState>,
}

#[derive(Debug, Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub attempts: Vec<Attempt>,
    pub merge_requests: Vec<PublishedMergeRequest>,
    pub events: Vec<Event>,
    pub source_pull_request: Option<artidezi_config::resolve::PullRequestSource>,
}

pub async fn create_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<CreateTaskRequest>,
) -> Result<Json<Task>, ApiError> {
    require_admin(&state, &headers)?;
    let task = with_orchestrator(&state, move |o| {
        let selection = TaskSelection {
            agent_profile: request.agent_profile,
            delivery_mode: request.delivery_mode,
        };
        Ok(o.create_manual_task(
            &request.project,
            &request.instructions,
            &request.created_by,
            &selection,
        )?)
    })
    .await?;
    Ok(Json(task))
}

pub async fn list_tasks(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Task>>, ApiError> {
    require_admin(&state, &headers)?;
    let tasks = with_state(&state, move |o| Ok(o.db.list_tasks(query.state)?)).await?;
    Ok(Json(tasks))
}

pub async fn get_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_admin(&state, &headers)?;
    let detail = with_state(&state, move |o| {
        Ok(TaskDetail {
            source_pull_request: artidezi_orchestrator::discovery::source_of(
                &o.db.get_task(&task_id)?,
            )?,
            task: o.db.get_task(&task_id)?,
            attempts: o.db.list_attempts(&task_id)?,
            merge_requests: o.db.list_merge_requests(&task_id)?,
            events: o.db.list_events(&task_id)?,
        })
    })
    .await?;
    Ok(Json(detail))
}

pub async fn request_revision(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(request): Json<RevisionRequest>,
) -> Result<Json<Attempt>, ApiError> {
    require_admin(&state, &headers)?;
    let attempt = with_orchestrator(&state, move |o| {
        Ok(o.request_revision(&task_id, &request.feedback, &request.actor)?)
    })
    .await?;
    Ok(Json(attempt))
}

pub async fn revise_proposal(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(request): Json<RevisionRequest>,
) -> Result<Json<Attempt>, ApiError> {
    require_admin(&state, &headers)?;
    let attempt = with_orchestrator(&state, move |o| {
        Ok(o.revise_proposal(&task_id, &request.feedback, &request.actor)?)
    })
    .await?;
    Ok(Json(attempt))
}

pub async fn close_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(request): Json<ActorRequest>,
) -> Result<Json<Task>, ApiError> {
    require_admin(&state, &headers)?;
    let task =
        with_orchestrator(&state, move |o| Ok(o.close_task(&task_id, &request.actor)?)).await?;
    Ok(Json(task))
}

#[derive(Debug, Serialize)]
pub struct StateResponse {
    pub state: TaskState,
}

pub async fn reconcile_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    require_admin(&state, &headers)?;
    let task_state = with_orchestrator(&state, move |o| Ok(o.reconcile_task(&task_id)?)).await?;
    Ok(Json(StateResponse { state: task_state }))
}

pub async fn tick(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers)?;
    let report = with_orchestrator(&state, move |o| Ok(o.tick()?)).await?;
    Ok(Json(serde_json::json!({
        "lost_attempts": report.lost_attempts,
        "reconciled": report.reconciled.iter().map(|(id, s)| serde_json::json!({"task": id, "state": s})).collect::<Vec<_>>(),
        "errors": report.errors,
        "discovered": report.discovered,
        "artifact_cleanup": report.artifact_cleanup,
    })))
}

#[derive(Deserialize)]
pub struct CleanupRequest {
    #[serde(default)]
    pub apply: bool,
}

pub async fn cleanup_artifacts(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<CleanupRequest>,
) -> Result<Json<artidezi_store::retention::CleanupReport>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_orchestrator(&state, move |o| {
            Ok(o.cleanup_artifacts_at(chrono::Utc::now(), request.apply)?)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct ScanRequest {
    pub project: Option<String>,
}

pub async fn scan_pull_requests(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<ScanRequest>,
) -> Result<Json<artidezi_orchestrator::discovery::DiscoveryReport>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_orchestrator(&state, move |o| {
            Ok(o.scan_pull_requests(request.project.as_deref(), true)?)
        })
        .await?,
    ))
}

pub async fn list_pull_requests(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<TrackedPullRequest>>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_state(&state, |o| Ok(o.db.list_pull_request_sources(None)?)).await?,
    ))
}

pub async fn resume_pull_request(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<ActorRequest>,
) -> Result<Json<TrackedPullRequest>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_orchestrator(&state, move |o| {
            Ok(o.resume_pull_request(&id, &request.actor)?)
        })
        .await?,
    ))
}

pub async fn pause_pull_request(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<ActorRequest>,
) -> Result<Json<TrackedPullRequest>, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(
        with_orchestrator(&state, move |o| {
            Ok(o.pause_pull_request(&id, &request.actor)?)
        })
        .await?,
    ))
}

/// Explicit retry: recover accepted publication without another model run.
pub async fn retry_publication(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(request): Json<ActorRequest>,
) -> Result<Json<Task>, ApiError> {
    require_admin(&state, &headers)?;
    let task = with_orchestrator(&state, move |o| {
        Ok(o.retry_publication(&task_id, &request.actor)?)
    })
    .await?;
    Ok(Json(task))
}

/// One attempt-specific human grant, recorded before the ordinary retry machinery runs.
pub async fn authorize_workflow_publication(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(request): Json<WorkflowPublicationRequest>,
) -> Result<Json<Task>, ApiError> {
    require_admin(&state, &headers)?;
    let task = with_orchestrator(&state, move |o| {
        Ok(o.authorize_workflow_publication(
            &task_id,
            &request.attempt_id,
            &request.attempt_record_sha256,
            &request.reason,
            &request.actor,
        )?)
    })
    .await?;
    Ok(Json(task))
}
