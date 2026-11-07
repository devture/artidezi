// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::{ApiError, SharedState, WorkerToken};

/// Rejects unauthenticated worker requests before their body is read.
pub async fn worker_middleware(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    worker_of(&state, request.headers())?;
    Ok(next.run(request).await)
}

/// Rejects unauthenticated admin requests before their body is read.
pub async fn admin_middleware(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    require_admin(&state, request.headers())?;
    Ok(next.run(request).await)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Constant-time comparison so token guessing is not timing-assisted.
fn equal(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The worker a token belongs to.
pub fn worker_of<'a>(
    state: &'a SharedState,
    headers: &HeaderMap,
) -> Result<&'a WorkerToken, ApiError> {
    let token = bearer(headers).ok_or_else(ApiError::forbidden)?;
    state
        .tokens
        .workers
        .iter()
        .find(|worker| equal(&worker.token, token))
        .ok_or_else(ApiError::forbidden)
}

/// Rejects a worker presenting another worker's name, which is usually a misconfigured worker file.
pub fn require_worker<'a>(
    state: &'a SharedState,
    headers: &HeaderMap,
    pool: Option<&str>,
    id: &str,
) -> Result<&'a WorkerToken, ApiError> {
    let worker = worker_of(state, headers)?;
    if worker.worker_id != id || pool.is_some_and(|pool| worker.worker_pool != pool) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!(
                "this token belongs to worker {}/{}",
                worker.worker_pool, worker.worker_id
            ),
        ));
    }
    Ok(worker)
}

pub fn require_admin(state: &SharedState, headers: &HeaderMap) -> Result<(), ApiError> {
    match bearer(headers) {
        Some(token) if equal(&state.tokens.admin, token) => Ok(()),
        _ => Err(ApiError::forbidden()),
    }
}
