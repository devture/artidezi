// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Optional browser interface. Its password and session cookies never authorize API routes.
mod access;
mod assets;
mod logs;
mod render;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use artidezi_config::Config;
use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::TaskSelection;
use artidezi_contract::{
    attempt::{ArtifactEntry, AttemptRecord},
    common::ArtifactRef,
    result::AgentResult,
};
use artidezi_store::model::*;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{ApiError, SharedState, with_orchestrator, with_state};
use access::Access;

const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone)]
pub struct UiConfig {
    secure_cookie: bool,
}

impl std::fmt::Debug for UiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiConfig")
            .field("secure_cookie", &self.secure_cookie)
            .finish_non_exhaustive()
    }
}

impl UiConfig {
    pub fn new(secure_cookie: bool) -> Self {
        Self { secure_cookie }
    }

    pub fn hash_password(password: &str) -> Result<String, &'static str> {
        access::hash_password(password)
    }

    pub fn validate_user(user: &artidezi_store::db::UiUser, config: &Config) -> Result<(), String> {
        access::validate_user(user, config)
    }

    fn cookie_name(&self) -> &'static str {
        if self.secure_cookie {
            "__Host-artidezi_session"
        } else {
            "artidezi_dev_session"
        }
    }
}

#[derive(Clone)]
struct Session {
    csrf: String,
    access: Access,
    password_change_required: bool,
    nav_counts: render::NavCounts,
}

#[derive(Default)]
struct BrowserState {
    /// Bounds password hashing load across the instance.
    login_attempts: VecDeque<Instant>,
    /// Keeps guessing at one account from locking out the others.
    account_attempts: HashMap<String, VecDeque<Instant>>,
}

#[derive(Clone)]
struct UiState {
    api: SharedState,
    config: UiConfig,
    browser: Arc<Mutex<BrowserState>>,
    log_counts: Arc<Mutex<logs::LineCache>>,
}

pub(super) fn router(api: SharedState, config: UiConfig) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/tasks") }))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/account", get(account))
        .route("/account/password", post(change_password))
        .route("/account/timezone", post(change_timezone))
        .route("/tasks", get(tasks).post(create_task))
        .route("/tasks/new", get(new_task))
        .route("/tasks/{id}", get(task_detail))
        .route("/tasks/{id}/instructions", get(task_instructions))
        .route("/tasks/{id}/events/{event}", get(task_event))
        .route("/tasks/{id}/revisions", post(revise))
        .route("/tasks/{id}/proposal-revisions", post(revise_proposal))
        .route("/tasks/{id}/close", post(close))
        .route("/tasks/{id}/retry-publication", post(retry_publication))
        .route("/tasks/{id}/attempts/{attempt}/logs/{sha}", get(log))
        .route("/tasks/{id}/attempts/{attempt}/patches/{sha}", get(patch))
        .route("/projects", get(projects))
        .route("/agent-profiles", get(agent_profiles))
        .route("/workers", get(workers))
        .route("/workers/drain", post(drain_worker))
        .route("/workers/maintenance", post(set_maintenance))
        .route("/incoming-changes", get(incoming_changes))
        .route("/incoming-changes/scan", post(scan_incoming))
        .route("/incoming-changes/{id}/resume", post(resume_source))
        .route("/incoming-changes/{id}/pause", post(pause_source))
        .route("/assets/{name}", get(assets::serve))
        // 32,000 Unicode characters can occupy 384 KB in a URL-encoded form.
        .layer(DefaultBodyLimit::max(512 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(UiState {
            api,
            config,
            browser: Arc::default(),
            log_counts: Arc::default(),
        })
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let cache = if response.status() == StatusCode::OK
        && response
            .extensions()
            .get::<assets::ImmutableAsset>()
            .is_some()
    {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

fn random_token() -> Result<String, UiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| {
        UiError(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to start a secure session.",
        ))
    })?;
    Ok(hex::encode(bytes))
}

fn cookie<'a>(state: &UiState, headers: &'a HeaderMap) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == state.config.cookie_name()).then_some(value)
        })
}

fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn auth_attempt_allowed(state: &UiState, account: &str) -> bool {
    let mut browser = state.browser.lock().unwrap_or_else(|p| p.into_inner());
    let browser = &mut *browser;
    let now = Instant::now();
    let recent = |at: &Instant| now.duration_since(*at) < Duration::from_secs(60);
    browser.login_attempts.retain(recent);
    browser.account_attempts.retain(|_, attempts| {
        attempts.retain(recent);
        !attempts.is_empty()
    });
    if browser.login_attempts.len() >= 120 {
        return false;
    }
    let attempts = browser
        .account_attempts
        .entry(account.to_owned())
        .or_default();
    if attempts.len() >= 20 {
        return false;
    }
    attempts.push_back(now);
    browser.login_attempts.push_back(now);
    true
}

fn session_scope(config: &UiConfig) -> String {
    // A distinct scope makes every former shared-password or users-file session unusable.
    let mut digest = Sha256::new();
    digest.update(b"artidezi-browser-sessions-v3\0");
    digest.update(config.cookie_name());
    hex::encode(digest.finalize())
}

fn visible_config(config: Arc<Config>, access: &Access, writable_only: bool) -> Config {
    let mut config = (*config).clone();
    config.ui_access = None;
    config.projects.retain(|name, _| {
        if writable_only {
            access.can_write(name)
        } else {
            access.can_read(name)
        }
    });
    let used_profiles: std::collections::BTreeSet<_> = config
        .projects
        .values()
        .flat_map(|project| project.allowed_agent_profiles.iter().cloned())
        .collect();
    config
        .agent_profiles
        .retain(|name, _| used_profiles.contains(name));
    let used_pools: std::collections::BTreeSet<_> = config
        .agent_profiles
        .values()
        .map(|profile| profile.worker_pool.clone())
        .collect();
    config
        .worker_pools
        .retain(|name, _| used_pools.contains(name));
    config
}

fn visible_data(access: &Access, sources: &mut Vec<TrackedPullRequest>, tasks: &mut Vec<Task>) {
    sources.retain(|source| access.can_read(&source.project));
    tasks.retain(|task| access.can_read(&task.project));
}

fn ui_task_read(db: &artidezi_store::Db, id: &str, access: &Access) -> Result<Task, ApiError> {
    let task = match ui_task(db, id) {
        Ok(task) => task,
        Err(error) if error.status == StatusCode::NOT_FOUND => {
            return Err(ApiError::not_found("Task not found."));
        }
        Err(error) => return Err(error),
    };
    if !access.can_read(&task.project) {
        return Err(ApiError::not_found("Task not found."));
    }
    Ok(task)
}

fn ui_task_write(db: &artidezi_store::Db, id: &str, access: &Access) -> Result<Task, ApiError> {
    let task = ui_task_read(db, id, access)?;
    if !access.can_write(&task.project) {
        return Err(ApiError::forbidden());
    }
    Ok(task)
}

fn ui_source_write(
    db: &artidezi_store::Db,
    id: &str,
    access: &Access,
) -> Result<TrackedPullRequest, ApiError> {
    let source = match db.get_pull_request_source(id) {
        Ok(source) => source,
        Err(artidezi_store::StoreError::NotFound { .. }) => {
            return Err(ApiError::not_found("Incoming change not found."));
        }
        Err(error) => return Err(error.into()),
    };
    if !access.can_write(&source.project) {
        return Err(ApiError::not_found("Incoming change not found."));
    }
    Ok(source)
}

fn fresh_access(
    o: &artidezi_orchestrator::Orchestrator,
    prior: &Access,
) -> Result<Access, ApiError> {
    fresh_db_access(&o.db, prior)
}

fn fresh_db_access(db: &artidezi_store::Db, prior: &Access) -> Result<Access, ApiError> {
    let user = db
        .get_ui_user(&prior.id)?
        .filter(|user| !user.disabled)
        .ok_or_else(ApiError::forbidden)?;
    Ok(Access::from_user(&user))
}

async fn session(state: &UiState, headers: &HeaderMap) -> Result<Option<Session>, UiError> {
    let id = cookie(state, headers)
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(token_digest);
    let scope = session_scope(&state.config);
    let stored = with_state(&state.api, move |o| {
        let stored =
            o.db.ui_session_csrf(id.as_deref(), &scope, chrono::Utc::now())?;
        let user = stored
            .as_ref()
            .and_then(|stored| stored.principal_id.as_deref())
            .map(|id| o.db.get_ui_user(id))
            .transpose()?
            .flatten();
        let Some((stored, user)) = stored.zip(user) else {
            return Ok(None);
        };
        if user.disabled {
            return Ok(None);
        }
        let access = Access::from_user(&user);
        let mut sources = o.db.list_pull_request_sources(None)?;
        let mut tasks = o.db.list_tasks(None)?;
        visible_data(&access, &mut sources, &mut tasks);
        Ok(Some(Session {
            csrf: stored.csrf,
            access,
            password_change_required: user.password_change_required,
            nav_counts: render::nav_counts(&sources, &tasks),
        }))
    })
    .await?;
    Ok(stored)
}

fn cookie_header(state: &UiState, value: &str, seconds: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={seconds}{}",
        state.config.cookie_name(),
        if state.config.secure_cookie {
            "; Secure"
        } else {
            ""
        }
    ))
    .expect("hex cookie value")
}

/// Browsers attach Origin on form POSTs. Also reject explicit cross-site Fetch Metadata.
/// Requiring Origin prevents login CSRF before there is an authenticated session.
fn same_origin(state: &UiState, headers: &HeaderMap) -> Result<(), UiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let scheme = if state.config.secure_cookie {
        "https"
    } else {
        "http"
    };
    if host.is_empty()
        || origin != format!("{scheme}://{host}")
        || headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return Err(UiError(ApiError::forbidden()));
    }
    Ok(())
}

async fn require_form(
    state: &UiState,
    headers: &HeaderMap,
    csrf: &str,
) -> Result<Session, UiError> {
    same_origin(state, headers)?;
    let session = session(state, headers)
        .await?
        .ok_or_else(|| UiError(ApiError::forbidden()))?;
    if !bool::from(session.csrf.as_bytes().ct_eq(csrf.as_bytes())) {
        return Err(UiError(ApiError::forbidden()));
    }
    Ok(session)
}

struct UiError(ApiError);
impl From<ApiError> for UiError {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}
impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let message = if self.0.status.is_server_error() {
            tracing::error!("UI request failed: {}", self.0.message);
            "Something went wrong. Please retry or check the service logs."
        } else {
            &self.0.message
        };
        (self.0.status, Html(render::error_page(message))).into_response()
    }
}

async fn login_page(State(state): State<UiState>, headers: HeaderMap) -> Result<Response, UiError> {
    if session(&state, &headers).await?.is_some() {
        return Ok(Redirect::to("/tasks").into_response());
    }
    Ok(Html(render::login(None)).into_response())
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    password: String,
}

async fn login(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, UiError> {
    same_origin(&state, &headers)?;
    if !auth_attempt_allowed(&state, &form.username) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Html(render::login(Some(
                "Too many attempts. Try again in a minute.",
            ))),
        )
            .into_response());
    }
    let username = form.username;
    let password = form.password;
    let user = with_state(&state.api, move |o| Ok(o.db.get_ui_user(&username)?)).await?;
    let access = tokio::task::spawn_blocking(move || {
        let user = user.filter(|user| !user.disabled);
        let valid = access::verify_password(
            &password,
            user.as_ref()
                .map_or(access::DUMMY_HASH, |user| &user.password_hash),
        );
        if valid {
            user.as_ref()
                .map(|user| (Access::from_user(user), user.revision))
        } else {
            None
        }
    })
    .await
    .map_err(|_| {
        UiError(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Login failed.",
        ))
    })?;
    let Some((access, revision)) = access else {
        return Ok((
            StatusCode::UNAUTHORIZED,
            Html(render::login(Some(
                "Those credentials didn’t match. Please try again.",
            ))),
        )
            .into_response());
    };
    let id = random_token()?;
    let csrf = random_token()?;
    let digest = token_digest(&id);
    let replaced = cookie(&state, &headers).map(token_digest);
    let scope = session_scope(&state.config);
    let principal_id = Some(access.id.clone());
    let saved = with_state(&state.api, move |o| {
        let now = chrono::Utc::now();
        let expires = now + chrono::Duration::seconds(SESSION_LIFETIME.as_secs() as i64);
        Ok(o.db.save_ui_session(
            &digest,
            &artidezi_store::db::UiSession {
                csrf,
                principal_id,
                verified_revision: Some(revision),
            },
            &scope,
            expires,
            replaced.as_deref(),
            now,
        )?)
    })
    .await?;
    if !saved {
        return Err(UiError(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many active sessions. Try again later.",
        )));
    }
    let mut response = Redirect::to("/tasks").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie_header(&state, &id, SESSION_LIFETIME.as_secs()),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct ActionForm {
    csrf: String,
}
async fn logout(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Result<Response, UiError> {
    require_form(&state, &headers, &form.csrf).await?;
    if let Some(id) = cookie(&state, &headers) {
        let digest = token_digest(id);
        with_state(&state.api, move |o| {
            o.db.delete_ui_session(&digest)?;
            Ok(())
        })
        .await?;
    }
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_header(&state, "", 0));
    Ok(response)
}

async fn account(State(state): State<UiState>, headers: HeaderMap) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    Ok(account_html(&session, None, None).into_response())
}

fn account_html(
    session: &Session,
    password_error: Option<&str>,
    timezone_error: Option<&str>,
) -> Html<String> {
    Html(render::account(
        &session.access,
        session,
        password_error,
        timezone_error,
    ))
}

#[derive(Deserialize)]
struct TimezoneForm {
    csrf: String,
    timezone: String,
}

async fn change_timezone(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<TimezoneForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let timezone = form.timezone.trim().to_owned();
    if timezone.len() > 64 || timezone.parse::<chrono_tz::Tz>().is_err() {
        let page = account_html(
            &session,
            None,
            Some("Enter a valid IANA time zone, such as Europe/Sofia."),
        );
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
    }
    let id = session.access.id.clone();
    let changed = with_state(&state.api, move |o| {
        Ok(o.db.change_ui_timezone(&id, &timezone)?)
    })
    .await?;
    if !changed {
        return Err(ApiError::forbidden().into());
    }
    Ok(Redirect::to("/account#timezone").into_response())
}

#[derive(Deserialize)]
struct PasswordForm {
    csrf: String,
    current_password: String,
    new_password: String,
    confirm_password: String,
}

async fn change_password(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    if !auth_attempt_allowed(&state, &session.access.id) {
        let page = account_html(
            &session,
            Some("Too many attempts. Try again in a minute."),
            None,
        );
        return Ok((StatusCode::TOO_MANY_REQUESTS, page).into_response());
    }
    if form.new_password != form.confirm_password {
        let page = account_html(&session, Some("New passwords did not match."), None);
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
    }
    if form.new_password == form.current_password {
        let page = account_html(&session, Some("Choose a different password."), None);
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
    }
    let id = session.access.id.clone();
    let user = with_state(&state.api, {
        let id = id.clone();
        move |o| Ok(o.db.get_ui_user(&id)?)
    })
    .await?
    .ok_or_else(|| UiError(ApiError::forbidden()))?;
    if user.disabled {
        return Err(ApiError::forbidden().into());
    }
    let current = form.current_password;
    let next = form.new_password;
    let old_hash = user.password_hash.clone();
    let verified = tokio::task::spawn_blocking(move || {
        if !access::verify_password(&current, &old_hash) {
            return Ok(None);
        }
        access::hash_password(&next).map(Some)
    })
    .await
    .map_err(|_| {
        UiError(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Password change failed.",
        ))
    })?;
    let new_hash = match verified {
        Ok(Some(hash)) => hash,
        Ok(None) => {
            let page = account_html(&session, Some("Current password was incorrect."), None);
            return Ok((StatusCode::UNAUTHORIZED, page).into_response());
        }
        Err(message) => {
            let page = account_html(&session, Some(message), None);
            return Ok((StatusCode::BAD_REQUEST, page).into_response());
        }
    };
    let old_hash = user.password_hash;
    let changed = with_state(&state.api, move |o| {
        Ok(o.db.change_ui_password(&id, &old_hash, &new_hash)?)
    })
    .await?;
    if !changed {
        return Err(ApiError::forbidden().into());
    }
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_header(&state, "", 0));
    Ok(response)
}

#[derive(Clone, Default, Deserialize)]
struct TaskQuery {
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    project: String,
    #[serde(default)]
    page: usize,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    notice: String,
}

async fn tasks(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(query): Query<TaskQuery>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let access = session.access.clone();
    let page_query = query.clone();
    let (config, tasks, usage) = with_state(&state.api, move |o| {
        let mut tasks = o.db.list_tasks(None)?;
        tasks.retain(|task| access.can_read(&task.project));
        let mut usage = std::collections::BTreeMap::new();
        for task in render::task_page(&tasks, &page_query) {
            let mut total = artidezi_contract::attempt::Usage {
                input_tokens: 0,
                output_tokens: 0,
            };
            let mut reported = false;
            for attempt in o.db.list_attempts(&task.id)? {
                let Some(reference) = attempt
                    .attempt_record
                    .as_ref()
                    .filter(|r| r.size <= 4 * 1024 * 1024)
                else {
                    continue;
                };
                let Ok(record): Result<AttemptRecord, _> = o.artifacts.get_json(reference) else {
                    continue;
                };
                if let Some(found) = record.harness.usage {
                    reported = true;
                    total.input_tokens = total.input_tokens.saturating_add(found.input_tokens);
                    total.output_tokens = total.output_tokens.saturating_add(found.output_tokens);
                }
            }
            if reported {
                usage.insert(task.id.clone(), total);
            }
        }
        Ok((o.config.clone(), tasks, usage))
    })
    .await?;
    let config = visible_config(config, &session.access, false);
    Ok(Html(render::tasks(
        &config,
        &tasks,
        &usage,
        &session,
        &query,
        config
            .projects
            .keys()
            .any(|name| session.access.can_write(name)),
    ))
    .into_response())
}

#[derive(Default, Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    agent_profile: String,
    #[serde(default)]
    delivery_mode: String,
}

async fn new_task(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(form): Query<CreateForm>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let config = with_state(&state.api, |o| Ok(o.config.clone())).await?;
    let config = visible_config(config, &session.access, true);
    if config.projects.is_empty() {
        return Err(ApiError::forbidden().into());
    }
    Ok(Html(render::new_task(&config, &session, &form, None)).into_response())
}

async fn create_task(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let config = with_state(&state.api, |o| Ok(o.config.clone())).await?;
    let config = visible_config(config, &session.access, true);
    if !session.access.can_write(&form.project) {
        return Err(ApiError::forbidden().into());
    }
    let selection = TaskSelection {
        agent_profile: (!form.agent_profile.is_empty()).then(|| form.agent_profile.clone()),
        delivery_mode: match form.delivery_mode.as_str() {
            "" => None,
            "merge_request" => Some(DeliveryMode::MergeRequest),
            "merge_request_with_automerge" => Some(DeliveryMode::MergeRequestWithAutomerge),
            "direct_push_fast_forward" => Some(DeliveryMode::DirectPushFastForward),
            _ => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Html(render::new_task(
                        &config,
                        &session,
                        &form,
                        Some("Choose a valid delivery mode."),
                    )),
                )
                    .into_response());
            }
        },
    };
    if form.instructions.trim().is_empty() || form.instructions.chars().count() > 32_000 {
        return Ok((
            StatusCode::BAD_REQUEST,
            Html(render::new_task(
                &config,
                &session,
                &form,
                Some("Describe the task in 1–32,000 characters."),
            )),
        )
            .into_response());
    }
    let project = form.project.clone();
    let instructions = form.instructions.trim().to_string();
    let access = session.access.clone();
    match with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        if !access.can_write(&project) {
            return Err(ApiError::forbidden());
        }
        Ok(o.create_manual_task(&project, &instructions, &access.actor(), &selection)?)
    })
    .await
    {
        Ok(task) => {
            Ok(Redirect::to(&format!("/tasks/{}?notice=created", task.number)).into_response())
        }
        Err(error) if error.status.is_client_error() => Ok((
            error.status,
            Html(render::new_task(
                &config,
                &session,
                &form,
                Some(&error.message),
            )),
        )
            .into_response()),
        Err(error) => Err(error.into()),
    }
}

struct AttemptView {
    attempt: Attempt,
    record: Option<AttemptRecord>,
    result: Option<AgentResult>,
    logs: Vec<logs::LogView>,
    missing_result_diagnostic: Option<String>,
    expired_input_bundles: usize,
    /// Why a queued attempt can't be scheduled now, if no connected worker can run it.
    unschedulable: Option<String>,
}
struct Detail {
    can_write: bool,
    selected_attempt: u32,
    source: Option<artidezi_config::resolve::PullRequestSource>,
    tracking: Option<TrackedPullRequest>,
    task: Task,
    attempts: Vec<AttemptView>,
    merge_requests: Vec<PublishedMergeRequest>,
    events: Vec<Event>,
}

async fn task_detail(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<TaskQuery>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let detail = load_detail(&state, id, query.attempt, &session.access).await?;
    Ok(Html(render::detail(&detail, &session, &query.notice, None)).into_response())
}

async fn load_detail(
    state: &UiState,
    id: String,
    selected_attempt: u32,
    access: &Access,
) -> Result<Detail, UiError> {
    let log_counts = state.log_counts.clone();
    let access = access.clone();
    let lease_ttl_seconds = state.api.lease_ttl.as_secs();
    Ok(with_state(&state.api, move |o| {
        let task = ui_task_read(&o.db, &id, &access)?;
        let can_write = access.can_write(&task.project);
        let id = task.id.clone();
        let mut budget = 8 * 1024 * 1024;
        let mut cache = log_counts.lock().unwrap_or_else(|p| p.into_inner());
        let attempts = o.db.list_attempts(&id)?;
        let selected_attempt = if selected_attempt == 0 {
            attempts.last().map(|a| a.number).unwrap_or(0)
        } else if attempts.iter().any(|a| a.number == selected_attempt) {
            selected_attempt
        } else {
            return Err(ApiError::not_found("Attempt not found."));
        };
        let attempts = attempts
            .into_iter()
            .map(|attempt| {
                let selected = attempt.number == selected_attempt;
                let record: Option<AttemptRecord> = attempt
                    .attempt_record
                    .as_ref()
                    .filter(|r| selected && r.size <= 4 * 1024 * 1024)
                    .and_then(|r| o.artifacts.get_json(r).ok());
                let result = attempt
                    .result
                    .as_ref()
                    .filter(|r| selected && r.size <= 256 * 1024)
                    .and_then(|r| o.artifacts.get_json(r).ok());
                let logs = record
                    .as_ref()
                    .map(|record| logs::describe(record, &o.artifacts, &mut cache, &mut budget))
                    .unwrap_or_default();
                let missing_result_diagnostic = record
                    .as_ref()
                    .filter(|record| {
                        record.result.is_none()
                            && attempt
                                .validation_problems
                                .iter()
                                .any(|problem| problem == "no result document")
                    })
                    .map(|record| render::missing_result_diagnostic(record, &o.artifacts));
                let expired_input_bundles = if selected {
                    let job: artidezi_contract::Job = o.artifacts.get_json(&attempt.job)?;
                    let mut count = 0;
                    for artifact in job.repositories.iter().flat_map(|repo| {
                        std::iter::once(&repo.snapshot_bundle)
                            .chain(repo.input_refs.iter().map(|input| &input.bundle))
                    }) {
                        if !o.artifacts.contains(artifact)
                            && o.db.artifact_was_deleted(&artifact.sha256)?
                        {
                            count += 1;
                        }
                    }
                    count
                } else {
                    0
                };
                let unschedulable = if selected && attempt.state == AttemptState::Queued {
                    let job: artidezi_contract::Job = o.artifacts.get_json(&attempt.job)?;
                    render::unschedulable_reason(
                        &job,
                        &attempt.worker_pool,
                        &o.db.list_workers()?,
                        lease_ttl_seconds,
                    )
                } else {
                    None
                };
                Ok(AttemptView {
                    attempt,
                    record,
                    result,
                    logs,
                    missing_result_diagnostic,
                    expired_input_bundles,
                    unschedulable,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let source = artidezi_orchestrator::discovery::source_of(&task)?;
        let tracking = source
            .as_ref()
            .map(|s| o.db.get_pull_request_source(&s.tracking_id))
            .transpose()?;
        Ok(Detail {
            can_write,
            selected_attempt,
            source,
            tracking,
            task,
            attempts,
            merge_requests: o.db.list_merge_requests(&id)?,
            events: o.db.list_events(&id)?,
        })
    })
    .await?)
}

fn ui_task(db: &artidezi_store::Db, id: &str) -> Result<Task, ApiError> {
    Ok(if let Ok(number) = id.parse::<i64>() {
        db.get_task_by_number(number)?
    } else {
        db.get_task(id)?
    })
}

async fn task_instructions(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let access = session.access;
    let task = with_state(&state.api, move |o| ui_task_read(&o.db, &id, &access)).await?;
    Ok(Redirect::to(&format!("/tasks/{}#instructions-content", task.number)).into_response())
}

async fn task_event(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path((id, event_id)): Path<(String, i64)>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let access = session.access.clone();
    let (config, task, event) = with_state(&state.api, move |o| {
        let task = ui_task_read(&o.db, &id, &access)?;
        let event =
            o.db.list_events(&task.id)?
                .into_iter()
                .find(|event| event.id == event_id)
                .ok_or_else(|| ApiError::not_found("Event not found for this task."))?;
        Ok((
            visible_config(o.config.clone(), &access, false),
            task,
            event,
        ))
    })
    .await?;
    Ok(Html(render::event(&config, &task, &event, &session)).into_response())
}

#[derive(Deserialize)]
struct RevisionForm {
    csrf: String,
    feedback: String,
}
async fn revise(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RevisionForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let result = if form.feedback.trim().is_empty() || form.feedback.chars().count() > 32_000 {
        Err(ApiError::bad_request(
            "Provide revision feedback in 1–32,000 characters.",
        ))
    } else {
        let id = id.clone();
        let feedback = form.feedback.clone();
        let access = session.access.clone();
        with_orchestrator(&state.api, move |o| {
            let access = fresh_access(o, &access)?;
            let id = ui_task_write(&o.db, &id, &access)?.id;
            let attempt = o.request_revision(&id, feedback.trim(), &access.actor())?;
            Ok(o.db.get_task(&attempt.task_id)?.number)
        })
        .await
    };
    match result {
        Ok(task_id) => {
            Ok(Redirect::to(&format!("/tasks/{task_id}?notice=revised")).into_response())
        }
        Err(error) if error.status.is_client_error() => {
            let detail = load_detail(&state, id, 0, &session.access).await?;
            Ok((
                error.status,
                Html(render::detail(
                    &detail,
                    &session,
                    "",
                    Some((&form.feedback, &error.message)),
                )),
            )
                .into_response())
        }
        Err(error) => Err(error.into()),
    }
}

async fn revise_proposal(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RevisionForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let result = if form.feedback.trim().is_empty() || form.feedback.chars().count() > 32_000 {
        Err(ApiError::bad_request(
            "Provide revision feedback in 1–32,000 characters.",
        ))
    } else {
        let id = id.clone();
        let feedback = form.feedback.clone();
        let access = session.access.clone();
        with_orchestrator(&state.api, move |o| {
            let access = fresh_access(o, &access)?;
            let id = ui_task_write(&o.db, &id, &access)?.id;
            let attempt = o.revise_proposal(&id, feedback.trim(), &access.actor())?;
            Ok(o.db.get_task(&attempt.task_id)?.number)
        })
        .await
    };
    match result {
        Ok(task_id) => {
            Ok(Redirect::to(&format!("/tasks/{task_id}?notice=revised")).into_response())
        }
        Err(error) if error.status.is_client_error() => {
            let detail = load_detail(&state, id, 0, &session.access).await?;
            Ok((
                error.status,
                Html(render::detail(
                    &detail,
                    &session,
                    "",
                    Some((&form.feedback, &error.message)),
                )),
            )
                .into_response())
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Deserialize)]
struct CloseForm {
    csrf: String,
    #[serde(default)]
    confirmed: bool,
}

async fn close(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CloseForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    if !form.confirmed {
        let access = session.access.clone();
        let (task, config) = with_state(&state.api, move |o| {
            Ok((
                ui_task_write(&o.db, &id, &access)?,
                visible_config(o.config.clone(), &access, false),
            ))
        })
        .await?;
        return Ok(Html(render::confirm_close(&task, &config, &session)).into_response());
    }
    let access = session.access.clone();
    let number = with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        let id = ui_task_write(&o.db, &id, &access)?.id;
        o.close_task(&id, &access.actor())?;
        Ok(o.db.get_task(&id)?.number)
    })
    .await?;
    Ok(Redirect::to(&format!("/tasks/{number}?notice=closed")).into_response())
}

async fn workers(State(state): State<UiState>, headers: HeaderMap) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let (config, mut tasks, mut attempts, workers, paused) = with_state(&state.api, |o| {
        let tasks = o.db.list_tasks(None)?;
        let mut attempts = Vec::new();
        for task in &tasks {
            attempts.extend(o.db.list_attempts(&task.id)?);
        }
        Ok((
            o.config.clone(),
            tasks,
            attempts,
            o.db.list_workers()?,
            o.db.discovery_paused()?,
        ))
    })
    .await?;
    tasks.retain(|task| session.access.can_read(&task.project));
    let visible_ids: std::collections::BTreeSet<_> =
        tasks.iter().map(|task| task.id.as_str()).collect();
    attempts.retain(|attempt| visible_ids.contains(attempt.task_id.as_str()));
    let listed_workers = config.worker_pools.clone();
    let config = visible_config(config, &session.access, false);
    Ok(Html(render::workers(
        &config,
        &listed_workers,
        &attempts,
        &workers,
        state.api.lease_ttl.as_secs(),
        paused,
        &session,
    ))
    .into_response())
}

#[derive(Deserialize)]
struct DrainWorkerForm {
    csrf: String,
    worker_pool: String,
    worker_id: String,
    drained: bool,
}

#[derive(Deserialize)]
struct MaintenanceForm {
    csrf: String,
    discovery_paused: bool,
}

async fn set_maintenance(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<MaintenanceForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let access = session.access;
    with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        if !access.is_admin() {
            return Err(ApiError::forbidden());
        }
        o.db.set_discovery_paused(form.discovery_paused, &access.actor())?;
        Ok(())
    })
    .await?;
    Ok(Redirect::to("/workers").into_response())
}

async fn drain_worker(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<DrainWorkerForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let access = session.access;
    with_state(&state.api, move |o| {
        let access = fresh_db_access(&o.db, &access)?;
        if !access.is_admin() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Instance administrator access required.",
            ));
        }
        o.db.drain_worker(
            &form.worker_pool,
            &form.worker_id,
            form.drained,
            &access.actor(),
        )?;
        Ok(())
    })
    .await?;
    Ok(Redirect::to("/workers").into_response())
}

async fn projects(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(query): Query<TaskQuery>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let (config, mut sources, mut tasks) = with_state(&state.api, |o| {
        Ok((
            o.config.clone(),
            o.db.list_pull_request_sources(None)?,
            o.db.list_tasks(None)?,
        ))
    })
    .await?;
    visible_data(&session.access, &mut sources, &mut tasks);
    let config = visible_config(config, &session.access, false);
    Ok(Html(render::projects(
        &config,
        &sources,
        &tasks,
        &session,
        &query,
        &session.access,
    ))
    .into_response())
}

async fn incoming_changes(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(query): Query<TaskQuery>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let (config, mut sources, mut tasks) = with_state(&state.api, |o| {
        Ok((
            o.config.clone(),
            o.db.list_pull_request_sources(None)?,
            o.db.list_tasks(None)?,
        ))
    })
    .await?;
    visible_data(&session.access, &mut sources, &mut tasks);
    let config = visible_config(config, &session.access, false);
    Ok(Html(render::incoming_changes(
        &config,
        &sources,
        &tasks,
        &session,
        &query,
        &session.access,
    ))
    .into_response())
}

async fn agent_profiles(
    State(state): State<UiState>,
    headers: HeaderMap,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let config = with_state(&state.api, |o| Ok(o.config.clone())).await?;
    let config = visible_config(config, &session.access, false);
    Ok(Html(render::agent_profiles(&config, &session)).into_response())
}

#[derive(Deserialize)]
struct IncomingActionForm {
    csrf: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: usize,
}

impl IncomingActionForm {
    fn query(&self) -> TaskQuery {
        TaskQuery {
            project: self.project.clone(),
            filter: self.filter.clone(),
            q: self.q.clone(),
            page: self.page,
            ..TaskQuery::default()
        }
    }
}

async fn scan_incoming(
    State(state): State<UiState>,
    headers: HeaderMap,
    Form(form): Form<IncomingActionForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let next = render::incoming_url(&form.query());
    let access = session.access;
    let report = with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        let project = (!form.project.is_empty()).then_some(form.project.as_str());
        match project {
            Some(name) if !access.can_write(name) => return Err(ApiError::forbidden()),
            None if !access.is_admin() => return Err(ApiError::forbidden()),
            _ => {}
        }
        Ok(o.scan_pull_requests(project, true)?)
    })
    .await?;
    if headers
        .get(axum::http::header::ACCEPT)
        .is_some_and(|accept| accept.as_bytes() == b"application/json")
    {
        return Ok(Json(report).into_response());
    }
    if !report.errors.is_empty() {
        return Err(UiError(ApiError::bad_request(report.errors.join("; "))));
    }
    Ok(Redirect::to(&next).into_response())
}

async fn resume_source(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<IncomingActionForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let next = render::incoming_url(&form.query());
    let access = session.access;
    with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        ui_source_write(&o.db, &id, &access)?;
        o.resume_pull_request(&id, &access.actor())?;
        Ok(())
    })
    .await?;
    Ok(Redirect::to(&next).into_response())
}

async fn pause_source(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<IncomingActionForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let next = render::incoming_url(&form.query());
    let access = session.access;
    with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        ui_source_write(&o.db, &id, &access)?;
        o.pause_pull_request(&id, &access.actor())?;
        Ok(())
    })
    .await?;
    Ok(Redirect::to(&next).into_response())
}

async fn log(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path((id, attempt_id, sha)): Path<(String, String, String)>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let artifact = load_text_artifact(&state, id, attempt_id, sha, &session.access).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=artidezi-log.txt",
            ),
        ],
        artifact.bytes,
    )
        .into_response())
}

struct TextArtifact {
    task: Task,
    attempt: Attempt,
    entry: ArtifactEntry,
    bytes: Vec<u8>,
}
async fn load_text_artifact(
    state: &UiState,
    id: String,
    attempt_id: String,
    sha: String,
    access: &Access,
) -> Result<TextArtifact, UiError> {
    let access = access.clone();
    Ok(with_state(&state.api, move |o| {
        let task = ui_task_read(&o.db, &id, &access)?;
        let id = task.id.clone();
        let attempt =
            o.db.list_attempts(&id)?
                .into_iter()
                .find(|a| a.id == attempt_id)
                .ok_or_else(|| ApiError::not_found("Attempt not found."))?;
        let reference = attempt
            .attempt_record
            .as_ref()
            .ok_or_else(|| ApiError::not_found("No logs yet."))?;
        if reference.size > 4 * 1024 * 1024 {
            return Err(ApiError::bad_request("Attempt record is too large."));
        }
        let record: AttemptRecord = o.artifacts.get_json(reference)?;
        let entry = record
            .artifacts
            .iter()
            .find(|a| {
                a.sha256 == sha
                    && (a.media_type.starts_with("text/") || a.media_type == "application/x-ndjson")
            })
            .ok_or_else(|| ApiError::not_found("Log not found."))?;
        if entry.size > 8 * 1024 * 1024 {
            return Err(ApiError::bad_request(
                "Log exceeds the browser download limit.",
            ));
        }
        let bytes = o.artifacts.get(&ArtifactRef {
            sha256: entry.sha256.clone(),
            size: entry.size,
        })?;
        Ok(TextArtifact {
            task,
            attempt,
            entry: entry.clone(),
            bytes,
        })
    })
    .await?)
}
async fn patch(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path((id, attempt_id, sha)): Path<(String, String, String)>,
) -> Result<Response, UiError> {
    let Some(session) = session(&state, &headers).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let artifact = load_text_artifact(&state, id, attempt_id, sha, &session.access).await?;
    if !artifact.entry.name.ends_with(".patch") {
        return Err(ApiError::not_found("Patch not found.").into());
    }
    Ok(Html(render::patch(&artifact, &session)).into_response())
}

async fn retry_publication(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CloseForm>,
) -> Result<Response, UiError> {
    let session = require_form(&state, &headers, &form.csrf).await?;
    let access = session.access;
    let number = with_orchestrator(&state.api, move |o| {
        let access = fresh_access(o, &access)?;
        let task = ui_task_write(&o.db, &id, &access)?;
        Ok(o.retry_publication(&task.id, &access.actor())?.number)
    })
    .await?;
    Ok(Redirect::to(&format!("/tasks/{number}")).into_response())
}
