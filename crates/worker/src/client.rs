// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Blocking HTTP client for the orchestrator's worker API.

use std::path::Path;

use artidezi_contract::common::ArtifactRef;
use artidezi_contract::wire::*;
use serde::de::DeserializeOwned;
use ureq::tls::{RootCerts, TlsConfig};

use crate::{Result, WorkerError};

pub struct Client {
    agent: ureq::Agent,
    heartbeat_agent: ureq::Agent,
    base_url: String,
    token: String,
}

/// Leaves room for an error response when the artifact itself is tiny.
const MIN_DOWNLOAD_LIMIT: u64 = 64 * 1024;

impl Client {
    pub fn new(base_url: &str, token: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        Self {
            agent,
            // Control requests must not inherit the much longer artifact-transfer timeout.
            heartbeat_agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                .tls_config(
                    TlsConfig::builder()
                        .root_certs(RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .into(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn fail(status: u16, body: &str, what: &str) -> WorkerError {
        let message = serde_json::from_str::<ErrorResponse>(body)
            .map(|e| e.error)
            .unwrap_or_else(|_| body.trim().to_owned());
        WorkerError::Http {
            operation: what.into(),
            status,
            message,
        }
    }

    fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        payload: &impl serde::Serialize,
        what: &str,
    ) -> Result<Option<T>> {
        self.post_json_with(&self.agent, path, payload, what)
    }

    fn post_json_with<T: DeserializeOwned>(
        &self,
        agent: &ureq::Agent,
        path: &str,
        payload: &impl serde::Serialize,
        what: &str,
    ) -> Result<Option<T>> {
        let mut response = agent
            .post(self.url(path))
            .header("Authorization", &self.auth())
            .send_json(payload)
            .map_err(|e| WorkerError::Invalid(format!("{what}: {e}")))?;
        let status = response.status().as_u16();
        if status == 204 {
            return Ok(None);
        }
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| WorkerError::Invalid(format!("{what}: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(Self::fail(status, &body, what));
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| WorkerError::Invalid(format!("{what}: bad JSON: {e}")))
    }

    pub fn lease(&self, request: &LeaseRequest) -> Result<Option<LeaseResponse>> {
        self.post_json_with(&self.heartbeat_agent, "/worker/v1/lease", request, "lease")
    }

    pub fn status(&self, request: &WorkerStatusRequest) -> Result<()> {
        self.post_json_with::<serde_json::Value>(
            &self.heartbeat_agent,
            "/worker/v1/status",
            request,
            "status",
        )
        .map(|_| ())
    }

    pub fn heartbeat(&self, attempt_id: &str, worker_id: &str) -> Result<()> {
        let request = HeartbeatRequest {
            worker_id: worker_id.to_owned(),
        };
        match self.post_json_with::<serde_json::Value>(
            &self.heartbeat_agent,
            &format!("/worker/v1/attempts/{attempt_id}/heartbeat"),
            &request,
            "heartbeat",
        ) {
            // This endpoint returns 409 only after authenticating the worker and finding that
            // the attempt is no longer leased. Network failures and other HTTP errors are
            // not revocation signals.
            Err(WorkerError::Http { status: 409, .. }) => Err(WorkerError::Cancelled),
            result => result.map(|_| ()),
        }
    }

    /// Downloads one artifact to `dest`, verifying its digest and size.
    pub fn download(&self, attempt_id: &str, artifact: &ArtifactRef, dest: &Path) -> Result<()> {
        let mut response = self
            .agent
            .get(self.url(&format!(
                "/worker/v1/attempts/{attempt_id}/artifacts/{}",
                artifact.sha256
            )))
            .header("Authorization", &self.auth())
            .call()
            .map_err(|e| WorkerError::Invalid(format!("download: {e}")))?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            // One byte over the size: reaching the limit exactly is an error, even at the end of the body.
            .limit(artifact.size.saturating_add(1).max(MIN_DOWNLOAD_LIMIT))
            .read_to_vec()
            .map_err(|e| WorkerError::Invalid(format!("download: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(Self::fail(
                status,
                &String::from_utf8_lossy(&bytes),
                "download",
            ));
        }
        use sha2::Digest;
        let digest = hex::encode(sha2::Sha256::digest(&bytes));
        if digest != artifact.sha256 || bytes.len() as u64 != artifact.size {
            return Err(WorkerError::Invalid(format!(
                "download: artifact {} is corrupt",
                artifact.sha256
            )));
        }
        std::fs::write(dest, &bytes).map_err(crate::io_err(dest))?;
        Ok(())
    }

    /// Uploads one staged artifact; returns the reference the orchestrator stored.
    pub fn upload(
        &self,
        attempt_id: &str,
        artifact: &ArtifactRef,
        path: &Path,
    ) -> Result<ArtifactRef> {
        let bytes = std::fs::read(path).map_err(crate::io_err(path))?;
        let mut response = self
            .agent
            .put(self.url(&format!(
                "/worker/v1/attempts/{attempt_id}/artifacts/{}",
                artifact.sha256
            )))
            .header("Authorization", &self.auth())
            .header("Content-Type", "application/octet-stream")
            .send(&bytes[..])
            .map_err(|e| WorkerError::Invalid(format!("upload: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| WorkerError::Invalid(format!("upload: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(Self::fail(status, &body, "upload"));
        }
        let parsed: UploadResponse = serde_json::from_str(&body)
            .map_err(|e| WorkerError::Invalid(format!("upload: bad JSON: {e}")))?;
        Ok(parsed.artifact)
    }

    pub fn record(&self, attempt_id: &str, request: &RecordRequest) -> Result<RecordResponse> {
        self.post_json(
            &format!("/worker/v1/attempts/{attempt_id}/record"),
            request,
            "record",
        )?
        .ok_or_else(|| WorkerError::Invalid("record: empty response".into()))
    }
}
