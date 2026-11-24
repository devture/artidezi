// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Fixed-origin HTTP, bounded conditional GETs and rate-limit handling. Typed adapter methods
//! own mutation endpoints; mutations are never automatically retried.

use artidezi_contract::time::{SignedDuration, Timestamp};
use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::API_VERSION;
use crate::{CiLogOutcome, ForgeError, Result};

#[derive(Clone)]
struct Cached {
    etag: String,
    body: String,
}

pub(super) struct Http {
    api: String,
    agent: ureq::Agent,
    cache: Mutex<BTreeMap<String, Cached>>,
    backoff: Mutex<Option<Timestamp>>,
}

impl Http {
    /// GitHub returns a one-minute signed URL. The installation bearer is sent only to the
    /// configured API origin, never to the redirected storage host.
    pub(super) fn job_log(
        &self,
        path: &str,
        token: &str,
        consume: impl FnMut(&[u8]),
    ) -> std::result::Result<u64, CiLogOutcome> {
        let started = std::time::Instant::now();
        let mut response = self
            .agent
            .get(format!("{}{path}", self.api))
            .header(
                "User-Agent",
                &format!("ArtiDezi/{}", artidezi_contract::app_version()),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Accept-Encoding", "identity")
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|_| CiLogOutcome::FetchFailed)?;
        let status = response.status().as_u16();
        if status != 302 {
            return Err(match status {
                401 | 403 => CiLogOutcome::PermissionDenied,
                404 | 410 => CiLogOutcome::MissingOrInaccessible,
                _ => CiLogOutcome::FetchFailed,
            });
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or(CiLogOutcome::FetchFailed)?;
        let uri: ureq::http::Uri = location.parse().map_err(|_| CiLogOutcome::FetchFailed)?;
        let host = uri.host().ok_or(CiLogOutcome::FetchFailed)?;
        let trusted = uri.scheme_str() == Some("https")
            && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
            && uri.port_u16().is_none_or(|port| port == 443)
            && (host.ends_with(".blob.core.windows.net")
                || host.ends_with(".githubusercontent.com"));
        #[cfg(any(test, feature = "test-fixtures"))]
        let trusted = trusted
            || (uri.scheme_str() == Some("http")
                && host == "127.0.0.1"
                && self.api.starts_with("http://127.0.0.1:")
                && uri.authority()
                    == self
                        .api
                        .parse::<ureq::http::Uri>()
                        .ok()
                        .and_then(|u| u.authority().cloned())
                        .as_ref());
        if !trusted {
            return Err(CiLogOutcome::FetchFailed);
        }
        // No Authorization header here. Redirect following stays disabled, and both wire and
        // decoded body are bounded by ureq's response reader and our explicit ceiling.
        let remaining = std::time::Duration::from_secs(30)
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(CiLogOutcome::FetchFailed)?;
        let download: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(remaining))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        response = download
            .get(location)
            .header(
                "User-Agent",
                &format!("ArtiDezi/{}", artidezi_contract::app_version()),
            )
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(|_| CiLogOutcome::FetchFailed)?;
        if response.status().as_u16() != 200 {
            return Err(CiLogOutcome::FetchFailed);
        }
        crate::ci_log::read_bounded(&mut response, consume)
    }
    pub fn new(api: &str) -> Self {
        Self {
            api: api.into(),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .http_status_as_error(false)
                .max_redirects(0)
                .build()
                .into(),
            cache: Mutex::new(BTreeMap::new()),
            backoff: Mutex::new(None),
        }
    }

    fn ready(&self) -> Result<()> {
        if let Some(retry_at) = *self.backoff.lock().unwrap()
            && retry_at > Timestamp::now()
        {
            return Err(ForgeError::RetryAfter {
                retry_at,
                rate_limited: true,
            });
        }
        Ok(())
    }

    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().clear();
    }

    pub fn retry_after(&self) -> Option<Timestamp> {
        *self.backoff.lock().unwrap()
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
        conditional: bool,
    ) -> Result<T> {
        self.ready()?;
        let cached = conditional
            .then(|| self.cache.lock().unwrap().get(path).cloned())
            .flatten();
        let mut request = self
            .agent
            .get(format!("{}{path}", self.api))
            .header(
                "User-Agent",
                &format!("ArtiDezi/{}", artidezi_contract::app_version()),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(token) = token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        if let Some(cached) = &cached {
            request = request.header("If-None-Match", &cached.etag);
        }
        let response = request.call().map_err(transport_error)?;
        if response.status().as_u16() == 304 {
            let cached = cached.ok_or_else(|| {
                ForgeError::Other("GitHub returned 304 without a cached representation".into())
            })?;
            return parse(&cached.body);
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let text = self.body(response, 200)?;
        let parsed = parse(&text)?;
        if conditional
            && let Some(etag) = etag
            && text.len() <= 1024 * 1024
        {
            let mut cache = self.cache.lock().unwrap();
            // Bounded acceleration only. Eviction never turns a failed request into cached success.
            if cache.len() >= 128
                || cache.values().map(|c| c.body.len()).sum::<usize>() + text.len()
                    > 16 * 1024 * 1024
            {
                cache.clear();
            }
            cache.insert(path.into(), Cached { etag, body: text });
        }
        Ok(parsed)
    }

    pub fn installation_token<T: DeserializeOwned>(
        &self,
        id: u64,
        jwt: &str,
        body: &Value,
    ) -> Result<T> {
        self.ready()?;
        let response = self
            .agent
            .post(format!("{}/app/installations/{id}/access_tokens", self.api))
            .header(
                "User-Agent",
                &format!("ArtiDezi/{}", artidezi_contract::app_version()),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Authorization", &format!("Bearer {jwt}"))
            .send_json(body)
            .map_err(transport_error)?;
        parse(&self.body(response, 201)?)
    }

    /// Called only by typed, policy-checked publication operations. Never retries a mutation.
    pub(super) fn mutate<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        token: &str,
        value: &Value,
        expected: u16,
    ) -> Result<T> {
        self.ready()?;
        let request = match method {
            "POST" => self.agent.post(format!("{}{path}", self.api)),
            "PATCH" => self.agent.patch(format!("{}{path}", self.api)),
            "PUT" => self.agent.put(format!("{}{path}", self.api)),
            _ => return Err(ForgeError::Other("unsupported GitHub mutation".into())),
        };
        // Even a lost response can represent a successful mutation; discard pre-effect reads.
        self.clear_cache();
        let response = request
            .header(
                "User-Agent",
                &format!("ArtiDezi/{}", artidezi_contract::app_version()),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Authorization", &format!("Bearer {token}"))
            .send_json(value)
            .map_err(transport_error)?;
        parse(&self.body(response, expected)?)
    }

    fn body(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
        expected: u16,
    ) -> Result<String> {
        let status = response.status().as_u16();
        let headers = response.headers();
        let retry_after = headers.get("retry-after").and_then(|h| h.to_str().ok());
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok());
        if matches!(status, 403 | 429) || remaining == Some("0") {
            let now = Timestamp::now();
            let seconds = retry_after
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|s| *s >= 0);
            let retry = seconds.and_then(|s| now.checked_add(SignedDuration::from_secs(s)));
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .and_then(Timestamp::from_unix_seconds);
            let retry_at = [Some(now + SignedDuration::from_secs(60)), retry, reset]
                .into_iter()
                .flatten()
                .max()
                .unwrap();
            *self.backoff.lock().unwrap() = Some(retry_at);
            if status != expected {
                let rate_limited = status == 429
                    || retry_after.is_some()
                    || remaining == Some("0")
                    // Secondary limits can come without either header.
                    || response
                        .body_mut()
                        .with_config()
                        .limit(64 * 1024)
                        .read_to_string()
                        .is_ok_and(|body| body.to_lowercase().contains("rate limit"));
                return Err(ForgeError::RetryAfter {
                    retry_at,
                    rate_limited,
                });
            }
        }
        if status == 401 {
            self.clear_cache();
            return Err(ForgeError::AuthenticationRejected);
        }
        if status != expected {
            if status == 404 {
                return Err(ForgeError::NotFound {
                    entity: "GitHub resource",
                    id: "absent or inaccessible".into(),
                });
            }
            let reason = match status {
                404 => {
                    "repository/request is absent or inaccessible; verify the installation and repository selection"
                }
                300..=399 => "redirect refused; use the repository's current canonical name",
                500..=599 => "temporary failure",
                _ => "request failed",
            };
            let message = format!("GitHub HTTP {status}: {reason}");
            return Err(if crate::temporary_status(status) {
                ForgeError::Temporary(message)
            } else {
                ForgeError::Other(message)
            });
        }
        response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| {
                let message = "GitHub response is unreadable or exceeds 8 MiB".into();
                if crate::connection_failed(&error) {
                    ForgeError::Temporary(message)
                } else {
                    ForgeError::Other(message)
                }
            })
    }
}

fn parse<T: DeserializeOwned>(body: &str) -> Result<T> {
    serde_json::from_str(body)
        .map_err(|_| ForgeError::Other("GitHub returned an unexpected response shape".into()))
}
fn transport_error(error: ureq::Error) -> ForgeError {
    let message =
        "GitHub request failed (transport or redirect); check connectivity and repository identity"
            .into();
    if crate::connection_failed(&error) {
        ForgeError::Temporary(message)
    } else {
        ForgeError::Other(message)
    }
}
