// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Periodic and human-driven housekeeping that does not belong to a single request.

use artidezi_store::model::*;
use chrono::{DateTime, Utc};

use crate::{Orchestrator, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub lost_attempts: Vec<String>,
    pub reconciled: Vec<(String, TaskState)>,
    pub errors: Vec<String>,
    pub discovered: Vec<String>,
    pub artifact_cleanup: Option<artidezi_store::retention::CleanupReport>,
}

impl Orchestrator {
    /// One maintenance pass: expire dead leases, resume due CI waits and reconcile proposals.
    pub fn tick(&mut self) -> Result<TickReport> {
        self.tick_at(Utc::now())
    }

    /// Ingest finishes within one exclusive call, so an attempt still uploaded here means a
    /// crash or panic during validation. Without this, its task would stay active forever.
    fn reject_unfinished_validations(&mut self) -> Result<()> {
        for attempt in self.db.attempts_in_state(AttemptState::Uploaded)? {
            let problems = vec!["validation did not finish".to_owned()];
            self.db.judge_attempt(&attempt.id, problems.clone())?;
            let task = self.db.get_task(&attempt.task_id)?;
            self.pause_task_source(
                &task,
                "Validation of the worker result did not finish; inspect it before resuming",
            )?;
            self.db.record_event(
                Some(&task.id),
                Some(&attempt.id),
                "orchestrator",
                "attempt.rejected",
                serde_json::json!({"problems": problems}),
            )?;
            if task.state != TaskState::NeedsHuman && !task.state.is_terminal() {
                self.db
                    .set_task_state(&task.id, TaskState::NeedsHuman, "orchestrator")?;
            }
        }
        Ok(())
    }

    pub fn tick_at(&mut self, now: DateTime<Utc>) -> Result<TickReport> {
        let mut report = TickReport::default();
        // First, so that a failure elsewhere in the tick never leaves the backup copy stale.
        if self.last_snapshot.is_none_or(|last| {
            let age = now.signed_duration_since(last).num_seconds();
            !(0..crate::SNAPSHOT_INTERVAL_SECONDS).contains(&age)
        }) {
            self.snapshot_database(now, &mut report);
        }
        report.lost_attempts = self.state_access().recover_expired_at(now)?;
        self.reject_unfinished_validations()?;
        if self.db.discovery_paused()? {
            return Ok(report);
        }
        // Indexed queue, independent of task state. Do this before reconciliation so a new inline
        // failure cannot be retried again in the same pass (including ticks with an injected time).
        for mr in self.db.due_branch_cleanups(now, 16)? {
            let task = self.db.get_task(&mr.task_id)?;
            match serde_json::from_str(&task.policy_snapshot) {
                Ok(policy) => self.cleanup_proposal_branch(&task, None, &policy, &mr, now),
                Err(error) => {
                    self.db.record_event(Some(&task.id), None, "orchestrator",
                        "merge_request.branch_delete_failed", serde_json::json!({
                            "repository": mr.repository_id, "number": mr.number,
                            "branch": mr.source_branch, "head": mr.head_commit, "reason": error.to_string()
                        }))?;
                    self.db.finish_branch_cleanup(&mr)?;
                }
            }
        }
        report.errors.extend(self.recover_held_execution()?);
        if let Some(policy) = &self.config.instance.artifact_retention
            && now
                .signed_duration_since(self.last_artifact_cleanup)
                .num_seconds()
                >= i64::from(policy.cleanup_interval_seconds)
        {
            self.last_artifact_cleanup = now;
            match self.cleanup_artifacts_at(now, true) {
                Ok(cleanup) => {
                    // A copy that still references deleted files would block later cleanups.
                    if cleanup.removed_bytes > 0
                        || cleanup.expired_bundle_files > 0
                        || cleanup.orphan_files > 0
                    {
                        self.snapshot_database(now, &mut report);
                    }
                    report.artifact_cleanup = Some(cleanup);
                }
                Err(error) => report.errors.push(format!("artifact cleanup: {error}")),
            }
        }
        for task in self.db.list_tasks(Some(TaskState::WaitingForCi))? {
            match self.resume_source_ci(&task, now) {
                Ok(state) => report.reconciled.push((task.id, state)),
                Err(e) => report.errors.push(format!("{}: {e}", task.id)),
            }
        }
        for task in self.db.list_tasks(Some(TaskState::NeedsHuman))? {
            match self.recheck_failed_source_checks(&task, now) {
                Ok(state) if state != task.state => report.reconciled.push((task.id, state)),
                Ok(_) => {}
                Err(e) => report.errors.push(format!("{}: {e}", task.id)),
            }
        }
        for task in self.db.list_tasks(Some(TaskState::AwaitingReview))? {
            match self.reconcile_task(&task.id) {
                Ok(state) => report.reconciled.push((task.id, state)),
                Err(e) => report.errors.push(format!("{}: {e}", task.id)),
            }
        }
        let discovery = self.scan_pull_requests_at(None, false, now)?;
        let follow_up = self.follow_up_pull_requests_at(now)?;
        self.persist_forge_backoffs()?;
        report.discovered = discovery.queued;
        report.discovered.extend(follow_up.queued);
        report.errors.extend(discovery.errors);
        report.errors.extend(follow_up.errors);
        Ok(report)
    }

    fn snapshot_database(&mut self, now: DateTime<Utc>, report: &mut TickReport) {
        self.last_snapshot = Some(now);
        if let Err(error) = self.db.snapshot_to(&self.snapshot_path) {
            report.errors.push(format!("database snapshot: {error}"));
        }
    }

    pub fn cleanup_artifacts_at(
        &mut self,
        now: DateTime<Utc>,
        apply: bool,
    ) -> Result<artidezi_store::retention::CleanupReport> {
        let policy = self
            .config
            .instance
            .artifact_retention
            .clone()
            .ok_or_else(|| {
                crate::OrchestratorError::Invalid(
                    "instance.artifact_retention is not configured".into(),
                )
            })?;
        let mut refresh_errors = Vec::new();
        let cutoff = now
            - chrono::Duration::seconds(i64::from(policy.completed_task_bundle_retention_seconds));
        for task in self.db.list_tasks(None)? {
            if !task.state.is_terminal()
                || (task.updated_at > cutoff && !self.db.task_inputs_expired(&task.id)?)
            {
                continue;
            }
            for mr in self.db.list_merge_requests(&task.id)? {
                if mr.state != MergeRequestState::Open {
                    continue;
                }
                if let Err(error) = self.refresh_retired_publication(&task, mr, now) {
                    refresh_errors.push(format!("{}: {error}", task.id));
                }
            }
        }
        self.persist_forge_backoffs()?;
        let mut report =
            artidezi_store::retention::cleanup(&mut self.db, &self.artifacts, &policy, now, apply)?;
        report.publication_refresh_errors = refresh_errors;
        Ok(report)
    }

    /// Observes retired proposals without invoking delivery, automerge, tagging or task transitions.
    fn refresh_retired_publication(
        &mut self,
        task: &Task,
        mut mr: PublishedMergeRequest,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let policy: artidezi_config::ResolvedProjectPolicy =
            serde_json::from_str(&task.policy_snapshot)
                .map_err(|e| crate::OrchestratorError::Invalid(e.to_string()))?;
        let repository = policy
            .repositories
            .get(mr.repository_id.as_str())
            .ok_or_else(|| {
                crate::OrchestratorError::Invalid(
                    "published repository is absent from frozen policy".into(),
                )
            })?;
        if repository.forge != mr.forge
            || repository.forge_api_base_url.as_ref().is_some_and(|url| {
                self.config
                    .forges
                    .get(&mr.forge)
                    .is_none_or(|forge| &forge.api_base_url != url)
            })
        {
            return Err(crate::OrchestratorError::Invalid(
                "published forge identity changed".into(),
            ));
        }
        let info = self
            .forge(&mr.forge)?
            .get_merge_request(&repository.remote, mr.number)?;
        mr.state = match info.state {
            artidezi_forge::MergeRequestState::Open => return Ok(()),
            artidezi_forge::MergeRequestState::Closed => MergeRequestState::Closed,
            artidezi_forge::MergeRequestState::Merged => MergeRequestState::Merged,
        };
        mr.updated_at = now;
        self.db.upsert_merge_request(&mr)?;
        if mr.state == MergeRequestState::Merged {
            self.cleanup_proposal_branch(task, None, &policy, &mr, now);
        }
        self.db.record_event(Some(&task.id), None, "orchestrator", "publication.retired_state_observed", serde_json::json!({"repository": mr.repository_id, "number": mr.number, "state": mr.state}))?;
        Ok(())
    }

    /// A human closes a task without delivery. Any running attempt is cancelled.
    pub fn close_task(&mut self, task_id: &str, actor: &str) -> Result<Task> {
        let task = self.db.get_task(task_id)?;
        self.pause_task_source(&task, "Paused by a human")?;
        if let Some(latest) = self.db.latest_attempt(task_id)?
            && !latest.state.is_terminal()
        {
            self.db.cancel_attempt(&latest.id, actor)?;
        }
        self.db.set_task_state(task_id, TaskState::Closed, actor)?;
        Ok(self.db.get_task(task_id)?)
    }
}
