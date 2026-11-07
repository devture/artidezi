// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Worker-facing routes.

use artidezi_contract::common::ArtifactRef;
use artidezi_contract::wire::*;
use artidezi_orchestrator::ingest::job_artifacts;
use artidezi_store::model::AttemptState;
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::Duration as ChronoDuration;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path as FsPath, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::auth::{require_worker, worker_of};
use crate::{ApiError, SharedState, WorkerToken, with_orchestrator, with_state};

fn chrono_ttl(state: &SharedState) -> ChronoDuration {
    ChronoDuration::seconds(state.lease_ttl.as_secs() as i64)
}

const MAX_WORKER_TAGS: usize = 64;

pub async fn status(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<WorkerStatusRequest>,
) -> Result<StatusCode, ApiError> {
    require_worker(
        &state,
        &headers,
        Some(&request.worker_pool),
        &request.worker_id,
    )?;
    with_state(&state, move |o| {
        o.db.observe_worker(
            &request.worker_pool,
            &request.worker_id,
            Some(request.quarantined),
            chrono::Utc::now(),
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn lease(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<LeaseRequest>,
) -> Result<axum::response::Response, ApiError> {
    require_worker(
        &state,
        &headers,
        Some(&request.worker_pool),
        &request.worker_id,
    )?;
    if !artidezi_contract::job::SUPPORTED_JOB_SCHEMA_VERSIONS
        .iter()
        .all(|version| request.supported_job_schema_versions.contains(version))
    {
        return Err(ApiError::bad_request(
            "worker must support job schema versions 1 and 2; upgrade the worker",
        ));
    }
    if request.tags.len() > MAX_WORKER_TAGS
        || !request
            .tags
            .iter()
            .all(|tag| tag.len() <= 128 && artidezi_contract::validate::is_plain_id(tag))
    {
        return Err(ApiError::bad_request(
            "tags must be at most 64 plain identifiers",
        ));
    }
    let ttl = chrono_ttl(&state);
    let ttl_seconds = state.lease_ttl.as_secs();
    let advertised = request.harnesses.clone();
    let leased = with_state(&state, move |o| {
        o.db.record_worker_offer(
            &request.worker_pool,
            &request.worker_id,
            &request.harnesses,
            &request.tags,
            chrono::Utc::now(),
        )?;
        let Some(work) = o.lease_compatible(
            &request.worker_pool,
            &request.worker_id,
            ttl,
            &request.harnesses,
            &request.tags,
        )?
        else {
            return Ok(None);
        };
        o.db.record_event(
            Some(&work.attempt.task_id),
            Some(&work.attempt.id),
            &request.worker_id,
            "attempt.leased",
            serde_json::json!({"harnesses": advertised}),
        )?;
        Ok(Some(LeaseResponse {
            attempt_id: work.attempt.id.as_str().into(),
            attempt_number: work.attempt.number,
            lease_ttl_seconds: ttl_seconds,
            job_artifact: work.attempt.job.clone(),
            job: work.job,
            task_document: String::from_utf8_lossy(&work.task_document).into_owned(),
        }))
    })
    .await?;
    Ok(match leased {
        Some(response) => Json(response).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// Checks that the attempt is leased to the calling worker and returns the job's artifact set.
fn authorize_attempt(
    o: &artidezi_orchestrator::state::StateAccess,
    attempt_id: &str,
    worker: &WorkerToken,
) -> Result<(artidezi_store::model::Attempt, Vec<ArtifactRef>), ApiError> {
    let attempt = o.db.get_attempt(attempt_id)?;
    if attempt.worker_pool != worker.worker_pool {
        return Err(ApiError::forbidden());
    }
    if attempt.state != AttemptState::Leased {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("attempt is {}", attempt.state.as_str()),
        ));
    }
    o.require_live_lease(&attempt, chrono::Utc::now())
        .map_err(|error| match error {
            artidezi_orchestrator::OrchestratorError::Store(
                artidezi_store::StoreError::NotFound {
                    entity: "lease", ..
                },
            ) => ApiError::new(StatusCode::CONFLICT, "attempt lease expired"),
            other => other.into(),
        })?;
    if o.db
        .get_lease(attempt_id)?
        .is_none_or(|lease| lease.worker_id != worker.worker_id)
    {
        return Err(ApiError::forbidden());
    }
    let job = o.load_job(&attempt)?;
    let mut refs = job_artifacts(&job);
    refs.push(attempt.job.clone());
    Ok((attempt, refs))
}

pub async fn heartbeat(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(attempt_id): Path<String>,
    Json(request): Json<HeartbeatRequest>,
) -> Result<StatusCode, ApiError> {
    let worker = require_worker(&state, &headers, None, &request.worker_id)?.clone();
    let ttl = chrono_ttl(&state);
    with_state(&state, move |o| {
        authorize_attempt(o, &attempt_id, &worker)?;
        o.heartbeat(&attempt_id, &request.worker_id, ttl)
            .map_err(|error| match error {
                // Cancellation or record acceptance can revoke the lease after authorization.
                artidezi_orchestrator::OrchestratorError::Store(
                    artidezi_store::StoreError::NotFound {
                        entity: "lease", ..
                    },
                ) => ApiError::new(
                    StatusCode::CONFLICT,
                    "attempt lease is no longer held by this worker",
                ),
                other => other.into(),
            })?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn download(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((attempt_id, sha256)): Path<(String, String)>,
) -> Result<Bytes, ApiError> {
    let worker = worker_of(&state, &headers)?.clone();
    let bytes = with_state(&state, move |o| {
        let (_, refs) = authorize_attempt(o, &attempt_id, &worker)?;
        let Some(artifact) = refs.into_iter().find(|r| r.sha256 == sha256) else {
            return Err(ApiError::not_found(
                "artifact is not part of this attempt's job",
            ));
        };
        Ok(o.artifacts.get(&artifact)?)
    })
    .await?;
    Ok(Bytes::from(bytes))
}

/// Removes a partial upload when the request fails. A no-op once the file has been adopted.
struct TempUpload(PathBuf);

impl Drop for TempUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn too_large(message: String) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, message)
}

/// Streams the body to `path`. Returns its SHA-256 and size.
async fn receive_upload(body: Body, path: &FsPath, limit: u64) -> Result<(String, u64), ApiError> {
    let io = |e: std::io::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let mut file = tokio::fs::File::create(path).await.map_err(io)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(e.to_string()))?;
        size += chunk.len() as u64;
        if size > limit {
            return Err(too_large(format!("upload is above the {limit} byte limit")));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(io)?;
    }
    file.sync_all().await.map_err(io)?;
    Ok((hex::encode(hasher.finalize()), size))
}

pub async fn upload(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((attempt_id, sha256)): Path<(String, String)>,
    body: Body,
) -> Result<Json<UploadResponse>, ApiError> {
    let worker = worker_of(&state, &headers)?.clone();
    let max_upload_bytes = state.limits.max_upload_bytes;
    let max_attempt_upload_bytes = state.limits.max_attempt_upload_bytes;
    let (early_id, early_worker) = (attempt_id.clone(), worker.clone());
    with_state(&state, move |o| {
        authorize_attempt(o, &early_id, &early_worker)?;
        Ok(())
    })
    .await?;
    let announced = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if announced.is_some_and(|size| size > max_upload_bytes) {
        return Err(too_large(format!(
            "upload is above the {max_upload_bytes} byte limit"
        )));
    }
    let _slot = state
        .upload_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let temp = TempUpload(state.access.artifacts.upload_temp_path());
    let (actual, size) = receive_upload(body, &temp.0, max_upload_bytes).await?;
    if actual != sha256 {
        return Err(ApiError::bad_request(
            "uploaded bytes do not match the sha256 in the path",
        ));
    }
    let temp_path = temp.0.clone();
    let artifact = with_state(&state, move |o| {
        let _guard = o.artifacts.upload_guard();
        authorize_attempt(o, &attempt_id, &worker)?;
        let artifact = ArtifactRef { sha256, size };
        let pinned = o.db.pin_artifact_upload_within(
            &attempt_id,
            &artifact,
            max_attempt_upload_bytes,
            chrono::Utc::now(),
        )?;
        if !pinned {
            return Err(too_large(format!(
                "attempt uploads are above the {max_attempt_upload_bytes} byte limit"
            )));
        }
        Ok(o.artifacts.adopt(&temp_path, &artifact)?)
    })
    .await?;
    Ok(Json(UploadResponse { artifact }))
}

pub async fn record(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(attempt_id): Path<String>,
    Json(request): Json<RecordRequest>,
) -> Result<Json<RecordResponse>, ApiError> {
    let worker = require_worker(&state, &headers, None, &request.worker_id)?.clone();
    let record_bytes = serde_json::to_vec_pretty(&request.record)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let slots = state
        .inspection_slots
        .get(&worker.worker_pool)
        .ok_or_else(ApiError::forbidden)?;
    let slot = slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let (id, inspected_worker, inspected_bytes) =
        (attempt_id.clone(), worker.clone(), record_bytes.clone());
    let inspection = with_state(&state, move |o| {
        authorize_attempt(o, &id, &inspected_worker)?;
        Ok(o.inspect_bundles(&id, &inspected_bytes)?)
    })
    .await?;
    drop(slot);
    let state_name = with_orchestrator(&state, move |o| {
        authorize_attempt(&o.state_access(), &attempt_id, &worker)?;
        let attempt_state = o.ingest_stored(
            &attempt_id,
            &request.worker_id,
            &record_bytes,
            request.artifacts,
            inspection,
        )?;
        Ok(attempt_state.as_str().to_owned())
    })
    .await?;
    Ok(Json(RecordResponse {
        attempt_state: state_name,
    }))
}
