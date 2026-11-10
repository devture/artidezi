// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use artidezi_contract::common::RepositoryAccess;
use artidezi_contract::{Job, attempt::AttemptRecord};
use artidezi_store::ArtifactStore;
use artidezi_store::model::{Attempt, AttemptState, Task, TaskState};

use crate::{Orchestrator, Result};

impl Orchestrator {
    pub(crate) fn retry_transient_execution(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        reason: &str,
        problems: &[String],
    ) -> Result<bool> {
        let mut superseded = false;
        if let Some(source) = crate::discovery::source_of(task)? {
            let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
            if tracked.paused {
                return Ok(false);
            }
            if tracked.observed_head != source.input.commit.as_str() {
                let error = crate::OrchestratorError::SourceHeadChanged {
                    expected: source.input.commit.clone(),
                    actual: tracked.observed_head.as_str().into(),
                };
                if !self.reschedule_superseded_source(task, attempt, &error)? {
                    return Ok(false);
                }
                superseded = true;
            }
        }
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "attempt.rejected",
            serde_json::json!({"problems":problems,"transient_failure":reason}),
        )?;
        if !superseded {
            self.db.record_transient_failure(&attempt.id, reason)?;
            self.state_access().recover_expired_at(chrono::Utc::now())?;
        }
        Ok(true)
    }

    /// Adopt recoverable holds left by an older controller or a restart. Explicit source pauses
    /// and publication plans remain barriers; an explicit retry can supersede an earlier pause.
    pub(crate) fn recover_held_execution(&mut self) -> Result<Vec<String>> {
        let mut errors = Vec::new();
        for task in self.db.list_tasks(Some(TaskState::NeedsHuman))? {
            if let Err(error) = self.recover_held_task(&task) {
                tracing::warn!(task = %task.id, %error, "automatic held-task recovery deferred");
                errors.push(format!("{}: automatic recovery: {error}", task.id));
            }
        }
        Ok(errors)
    }

    fn recover_held_task(&mut self, task: &Task) -> Result<()> {
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(());
        };
        let events = self.db.list_events(&task.id)?;
        let last_pause = events
            .iter()
            .filter(|event| event.kind == "discovery.paused")
            .map(|event| event.id)
            .max()
            .unwrap_or(0);
        let last_retry = events
            .iter()
            .filter(|event| event.kind == "publication.retry_requested")
            .map(|event| event.id)
            .max()
            .unwrap_or(0);
        let mut source_moved = false;
        if let Some(source) = crate::discovery::source_of(task)? {
            let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
            if tracked.last_task_id.as_deref() != Some(&task.id)
                || (tracked.paused && last_pause > last_retry)
            {
                return Ok(());
            }
            source_moved = tracked.observed_head != source.input.commit.as_str();
        }
        if attempt.state == AttemptState::Invalid
            && attempt.result.is_none()
            && !self.db.transient_failure_recorded(&attempt.id)?
            && self
                .config
                .worker_pools
                .get(&attempt.worker_pool)
                .is_some_and(|pool| pool.transient_failure_retries > 0)
        {
            let Some(reference) = &attempt.attempt_record else {
                return Ok(());
            };
            let record: AttemptRecord = self.artifacts.get_json(reference)?;
            let job = self.load_job(&attempt)?;
            if record.attempt_id.as_str() != attempt.id || record.job != attempt.job {
                return Ok(());
            }
            let Some(reason) =
                transient_failure(&self.artifacts, &job, &record, &attempt.validation_problems)
            else {
                return Ok(());
            };
            if let Some(source) = crate::discovery::source_of(task)? {
                let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
                if tracked.observed_head != source.input.commit.as_str() {
                    let error = crate::OrchestratorError::SourceHeadChanged {
                        expected: source.input.commit.clone(),
                        actual: tracked.observed_head.as_str().into(),
                    };
                    self.reschedule_superseded_source(task, &attempt, &error)?;
                    return Ok(());
                }
                tracked.paused = false;
                tracked.status = "Transient model failure; automatic retry scheduled".into();
                self.db.save_pull_request_source(&tracked)?;
            }
            self.db.adopt_transient_failure(&attempt.id, reason)?;
            self.state_access().recover_expired_at(chrono::Utc::now())?;
        } else if attempt.state == AttemptState::Valid
            && events
                .iter()
                .rev()
                .find(|event| event.kind == "task.needs_human")
                .and_then(|event| event.payload["reason"].as_str())
                .is_some_and(|reason| reason.contains("source head changed from"))
            && !events.iter().any(|event| {
                (event.kind.starts_with("publication.")
                    && event.kind != "publication.retry_requested")
                    || event.kind.starts_with("assessment.")
            })
            && self.db.list_merge_requests(&task.id)?.is_empty()
        {
            // The existing publication path checks authority/freshness first and retires
            // an obsolete result before any write. Never bypass its reconciliation checks.
            self.retry_publication(&task.id, "recovery")?;
        } else if source_moved
            && attempt.state.is_terminal()
            && !self.db.list_merge_requests(&task.id)?.is_empty()
        {
            self.retire_superseded_adaptations(task)?;
        }
        Ok(())
    }
}

/// Only incomplete execution with a terminal provider failure qualifies. Never retry forged
/// identities, prohibited repository changes, invalid result documents or failed verification.
pub(crate) fn transient_failure(
    store: &ArtifactStore,
    job: &Job,
    record: &AttemptRecord,
    problems: &[String],
) -> Option<&'static str> {
    if job.agent_profile.harness != artidezi_contract::job::Harness::Codex
        || record.harness.name != "codex"
        || record.result.is_some()
        || !record.harness.exit_code.is_some_and(|code| code != 0)
    {
        return None;
    }
    if !problems
        .iter()
        .any(|problem| problem == "no result document")
        || !problems.iter().all(|problem| {
            problem == "no result document"
                || job
                    .repositories
                    .iter()
                    .filter(|repo| repo.access == RepositoryAccess::ReadWrite)
                    .any(|repo| {
                        problem
                            == &format!("repository {}: reported unchanged but HEAD moved", repo.id)
                    })
        })
    {
        return None;
    }
    let reference = record.harness.stdout.as_ref()?;
    if !record.artifacts.iter().any(|entry| {
        entry.name == "harness-stdout.log"
            && entry.sha256 == reference.sha256
            && entry.size == reference.size
    }) {
        return None;
    }
    let bytes = store.tail(reference, 64 * 1024).ok()?;
    artidezi_contract::transient::codex_failure(&bytes)
}
