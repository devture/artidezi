// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use artidezi_contract::common::{ArtifactRef, Outcome};
use chrono::{DateTime, Duration, Utc};

use crate::Result;
use crate::model::*;

mod connection;
use connection::ConnectionDb;
pub use connection::sessions::UiSession;
pub use connection::ui_users::{UiAccountDeclaration, UiGrant, UiUser};

/// Cloneable access to the instance's single SQLite connection. Each operation holds the lock
/// only for its SQL queries/transaction; callers never hold it over Git, forge or artifact I/O.
/// The private implementation can call its own helpers without recursively acquiring this lock.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<ConnectionDb>>,
}

impl Db {
    pub fn adopt_transient_failure(&mut self, attempt_id: &str, reason: &str) -> Result<bool> {
        self.connection()
            .adopt_transient_failure(attempt_id, reason)
    }
    pub fn transient_failure_recorded(&self, attempt_id: &str) -> Result<bool> {
        self.connection().transient_failure_recorded(attempt_id)
    }
    pub fn discovery_paused(&self) -> Result<bool> {
        self.connection().discovery_paused()
    }
    pub fn set_discovery_paused(&mut self, paused: bool, actor: &str) -> Result<()> {
        self.connection().set_discovery_paused(paused, actor)
    }
    pub fn record_transient_failure(&self, attempt_id: &str, reason: &str) -> Result<()> {
        self.connection()
            .record_transient_failure(attempt_id, reason)
    }
    pub fn pending_transient_failures(&self) -> Result<Vec<Attempt>> {
        self.connection().pending_transient_failures()
    }
    pub fn recover_transient_attempt(
        &mut self,
        failed: &str,
        replacement: &Attempt,
        max_retries: u32,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.connection()
            .recover_transient_attempt(failed, replacement, max_retries, now)
    }
    pub fn observe_worker(
        &self,
        pool: &str,
        id: &str,
        quarantined: Option<bool>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.connection().observe_worker(pool, id, quarantined, now)
    }

    pub fn record_worker_offer(
        &self,
        pool: &str,
        id: &str,
        harnesses: &[artidezi_contract::wire::AdvertisedHarness],
        tags: &[String],
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.connection()
            .record_worker_offer(pool, id, harnesses, tags, now)
    }
    pub fn forget_unlisted_workers(&self, listed: &[(&str, &str)]) -> Result<()> {
        self.connection().forget_unlisted_workers(listed)
    }

    pub fn list_workers(&self) -> Result<Vec<Worker>> {
        self.connection().list_workers()
    }

    pub fn drain_worker(&mut self, pool: &str, id: &str, drained: bool, actor: &str) -> Result<()> {
        self.connection().drain_worker(pool, id, drained, actor)
    }

    pub fn pending_lost_attempts(&self) -> Result<Vec<Attempt>> {
        self.connection().pending_lost_attempts()
    }

    pub fn recover_lost_attempt(
        &mut self,
        lost: &str,
        replacement: Option<&Attempt>,
        max_retries: u32,
    ) -> Result<bool> {
        self.connection()
            .recover_lost_attempt(lost, replacement, max_retries)
    }
    pub fn task_inputs_expired(&self, task_id: &str) -> Result<bool> {
        self.connection().task_inputs_expired(task_id)
    }

    pub fn expire_task_inputs(&self, task_id: &str, now: DateTime<Utc>) -> Result<()> {
        self.connection().expire_task_inputs(task_id, now)
    }
    pub fn retention_metadata(&self) -> Result<Vec<serde_json::Value>> {
        self.connection().retention_metadata()
    }

    pub fn pin_artifact_upload(
        &self,
        attempt_id: &str,
        artifact: &ArtifactRef,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.connection()
            .pin_artifact_upload(attempt_id, artifact, now)
    }

    pub fn pin_artifact_upload_within(
        &self,
        attempt_id: &str,
        artifact: &ArtifactRef,
        max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.connection()
            .pin_artifact_upload_within(attempt_id, artifact, max_bytes, now)
    }

    pub fn artifact_uploads(&self) -> Result<Vec<(String, ArtifactRef, DateTime<Utc>)>> {
        self.connection().artifact_uploads()
    }

    pub fn artifact_was_deleted(&self, sha256: &str) -> Result<bool> {
        self.connection().artifact_was_deleted(sha256)
    }

    pub fn record_artifact_deletion(
        &self,
        artifact: &ArtifactRef,
        now: DateTime<Utc>,
        reason: &str,
    ) -> Result<()> {
        self.connection()
            .record_artifact_deletion(artifact, now, reason)
    }

    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(ConnectionDb::open(path)?)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(ConnectionDb::open_in_memory()?)),
        })
    }

    fn connection(&self) -> MutexGuard<'_, ConnectionDb> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn list_pull_request_sources(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<TrackedPullRequest>> {
        self.connection().list_pull_request_sources(project)
    }

    pub fn get_pull_request_source(&self, id: &str) -> Result<TrackedPullRequest> {
        self.connection().get_pull_request_source(id)
    }

    pub fn save_pull_request_source(&self, source: &TrackedPullRequest) -> Result<()> {
        self.connection().save_pull_request_source(source)
    }

    pub fn insert_discovery_task(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        source: &TrackedPullRequest,
    ) -> Result<()> {
        self.connection()
            .insert_discovery_task(task, attempt, source)
    }

    pub fn defer_forge(&self, forge: &str, retry_after: DateTime<Utc>) -> Result<()> {
        self.connection().defer_forge(forge, retry_after)
    }

    pub fn forge_retry_after(&self, forge: &str) -> Result<Option<DateTime<Utc>>> {
        self.connection().forge_retry_after(forge)
    }

    pub fn discovery_scan_due(
        &mut self,
        project: &str,
        interval_seconds: u32,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<bool> {
        self.connection()
            .discovery_scan_due(project, interval_seconds, now, force)
    }

    pub fn insert_task(&mut self, task: &Task) -> Result<()> {
        self.connection().insert_task(task)
    }

    pub fn get_task(&self, id: &str) -> Result<Task> {
        self.connection().get_task(id)
    }

    pub fn get_task_by_number(&self, number: i64) -> Result<Task> {
        self.connection().get_task_by_number(number)
    }

    pub fn list_tasks(&self, state: Option<TaskState>) -> Result<Vec<Task>> {
        self.connection().list_tasks(state)
    }

    pub fn set_task_state(&mut self, id: &str, next: TaskState, actor: &str) -> Result<()> {
        self.connection().set_task_state(id, next, actor)
    }

    pub fn insert_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        self.connection().insert_attempt(attempt)
    }

    pub fn wait_for_source_ci(
        &mut self,
        task_id: &str,
        next_check_at: DateTime<Utc>,
    ) -> Result<()> {
        self.connection().wait_for_source_ci(task_id, next_check_at)
    }

    pub fn claim_source_ci_check(
        &mut self,
        task_id: &str,
        now: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.connection()
            .claim_source_ci_check(task_id, now, next_check_at)
    }

    pub fn get_attempt(&self, id: &str) -> Result<Attempt> {
        self.connection().get_attempt(id)
    }

    pub fn list_attempts(&self, task_id: &str) -> Result<Vec<Attempt>> {
        self.connection().list_attempts(task_id)
    }

    pub fn attempts_in_state(&self, state: AttemptState) -> Result<Vec<Attempt>> {
        self.connection().attempts_in_state(state)
    }

    pub fn latest_attempt(&self, task_id: &str) -> Result<Option<Attempt>> {
        self.connection().latest_attempt(task_id)
    }

    /// Like [`Self::lease_next_attempt`], skipping queued attempts that `accepts` declines.
    pub fn lease_next_attempt_matching(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
        accepts: impl Fn(&Attempt) -> bool,
    ) -> Result<Option<(Attempt, Lease)>> {
        self.connection()
            .lease_next_attempt_matching(worker_pool, worker_id, ttl, now, accepts)
    }

    pub fn lease_next_attempt(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<(Attempt, Lease)>> {
        self.connection()
            .lease_next_attempt(worker_pool, worker_id, ttl, now)
    }

    pub fn get_lease(&self, attempt_id: &str) -> Result<Option<Lease>> {
        self.connection().get_lease(attempt_id)
    }

    pub fn heartbeat(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease> {
        self.connection().heartbeat(attempt_id, worker_id, ttl, now)
    }

    /// Renews a lease without extending it beyond the immutable job's execution deadline.
    pub fn heartbeat_until(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<Lease> {
        self.connection()
            .heartbeat_until(attempt_id, worker_id, ttl, now, deadline)
    }

    pub fn expire_leases(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        self.connection().expire_leases(now)
    }

    pub fn record_upload(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        record: &ArtifactRef,
        result: Option<&ArtifactRef>,
        outcome: Option<Outcome>,
    ) -> Result<()> {
        self.connection()
            .record_upload(attempt_id, worker_id, record, result, outcome)
    }

    pub fn judge_attempt(
        &mut self,
        attempt_id: &str,
        problems: Vec<String>,
    ) -> Result<AttemptState> {
        self.connection().judge_attempt(attempt_id, problems)
    }

    /// Atomically cancel only queued work. A worker which leased it first may finish.
    pub fn cancel_queued_attempt(&mut self, attempt_id: &str, actor: &str) -> Result<bool> {
        self.connection().cancel_queued_attempt(attempt_id, actor)
    }

    pub fn cancel_attempt(&mut self, attempt_id: &str, actor: &str) -> Result<()> {
        self.connection().cancel_attempt(attempt_id, actor)
    }

    pub fn upsert_merge_request(&mut self, mr: &PublishedMergeRequest) -> Result<()> {
        self.connection().upsert_merge_request(mr)
    }

    pub fn get_merge_request(
        &self,
        task_id: &str,
        repository_id: &str,
    ) -> Result<Option<PublishedMergeRequest>> {
        self.connection().get_merge_request(task_id, repository_id)
    }

    pub fn list_merge_requests(&self, task_id: &str) -> Result<Vec<PublishedMergeRequest>> {
        self.connection().list_merge_requests(task_id)
    }

    pub fn record_event(
        &self,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        actor: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64> {
        self.connection()
            .record_event(task_id, attempt_id, actor, kind, payload)
    }

    pub fn list_events(&self, task_id: &str) -> Result<Vec<Event>> {
        self.connection().list_events(task_id)
    }

    pub fn ui_session_csrf(
        &self,
        token_digest: Option<&str>,
        scope: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<UiSession>> {
        self.connection().ui_session_csrf(token_digest, scope, now)
    }

    pub fn save_ui_session(
        &mut self,
        token_digest: &str,
        session: &UiSession,
        scope: &str,
        expires_at: DateTime<Utc>,
        replaced_digest: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.connection().save_ui_session(
            token_digest,
            session,
            scope,
            expires_at,
            replaced_digest,
            now,
        )
    }

    pub fn delete_ui_session(&self, token_digest: &str) -> Result<()> {
        self.connection().delete_ui_session(token_digest)
    }

    pub fn get_ui_user(&self, id: &str) -> Result<Option<UiUser>> {
        self.connection().get_ui_user(id)
    }

    pub fn reconcile_ui_accounts(&mut self, desired: &[UiAccountDeclaration]) -> Result<()> {
        self.connection().reconcile_ui_accounts(desired)
    }

    pub fn list_ui_users(&self) -> Result<Vec<UiUser>> {
        self.connection().list_ui_users()
    }

    pub fn create_ui_user(&self, user: &UiUser) -> Result<()> {
        self.connection().create_ui_user(user)
    }

    pub fn update_ui_user(&mut self, user: &UiUser) -> Result<bool> {
        self.connection().update_ui_user(user)
    }

    pub fn change_ui_password(
        &mut self,
        id: &str,
        expected_hash: &str,
        new_hash: &str,
    ) -> Result<bool> {
        self.connection()
            .change_ui_password(id, expected_hash, new_hash)
    }

    pub fn change_ui_timezone(&mut self, id: &str, timezone: &str) -> Result<bool> {
        self.connection().change_ui_timezone(id, timezone)
    }
}
