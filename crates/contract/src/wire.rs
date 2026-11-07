// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Request and response bodies of the worker HTTP API. Bearer tokens travel in the
//! `Authorization` header; artifacts travel as raw bodies addressed by their SHA-256.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Job;
use crate::common::{ArtifactRef, AttemptId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedHarness {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub worker_pool: String,
    pub worker_id: String,
    pub supported_job_schema_versions: Vec<u32>,
    pub harnesses: Vec<AdvertisedHarness>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One leased attempt. Every artifact the job references is downloadable from
/// `GET /worker/v1/attempts/{attempt_id}/artifacts/{sha256}` for the lease's lifetime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub attempt_id: AttemptId,
    pub attempt_number: u32,
    pub lease_ttl_seconds: u64,
    pub job: Job,
    /// The exact bytes of `job.json` as stored, so the worker can hash-check them.
    pub job_artifact: ArtifactRef,
    pub task_document: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub worker_id: String,
}

/// Additive supervisor presence endpoint; no job or prompt schema change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStatusRequest {
    pub worker_pool: String,
    pub worker_id: String,
    pub quarantined: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UploadResponse {
    pub artifact: ArtifactRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordRequest {
    pub worker_id: String,
    /// `attempt.json`, embedded as-is.
    pub record: serde_json::Value,
    /// Artifacts uploaded before this request, by the names used in the record.
    pub artifacts: BTreeMap<String, ArtifactRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordResponse {
    /// `valid` or `invalid`.
    pub attempt_state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
