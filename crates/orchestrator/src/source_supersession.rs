// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Retire an unpublished result for an obsolete source head. Never replay that result or reset
//! its source budget; discovery must freeze and assess the new revision before any publication.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_contract::{
    common::{CommitSha, Outcome},
    result::AgentResult,
};
use artidezi_store::model::*;
use chrono::{Duration, Utc};

use crate::{Orchestrator, OrchestratorError, Result};

impl Orchestrator {
    pub(crate) fn reschedule_superseded_source(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        error: &OrchestratorError,
    ) -> Result<bool> {
        let OrchestratorError::SourceHeadChanged { expected, actual } = error else {
            return Ok(false);
        };
        // Ingestion's caller still holds the pre-upload lease record. Require the accepted
        // state and artifact references from durable storage rather than that earlier copy.
        let attempt = self.db.get_attempt(&attempt.id)?;
        if !matches!(
            task.state,
            TaskState::Working | TaskState::WaitingForCi | TaskState::NeedsHuman
        ) || attempt.task_id != task.id
        {
            return Ok(false);
        }
        let incomplete = attempt.state == AttemptState::Invalid
            && attempt.result.is_none()
            && self
                .config
                .worker_pools
                .get(&attempt.worker_pool)
                .is_some_and(|pool| pool.transient_failure_retries > 0)
            && if let Some(reference) = &attempt.attempt_record {
                let record = self.artifacts.get_json(reference)?;
                let job = self.load_job(&attempt)?;
                crate::recovery::transient_failure(
                    &self.artifacts,
                    &job,
                    &record,
                    &attempt.validation_problems,
                )
                .is_some()
            } else {
                false
            };
        let accepted = if attempt.state == AttemptState::Valid
            && let Some(reference) = &attempt.result
        {
            let result: AgentResult = self.artifacts.get_json(reference)?;
            matches!(
                result.outcome,
                Outcome::ApproveUntouched | Outcome::Proposal
            )
        } else {
            false
        };
        if !accepted && !incomplete {
            return Ok(false);
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        self.require_source_authority(task, &policy)?;
        let source = policy
            .source_pull_request
            .as_ref()
            .expect("checked authority");
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if (tracked.paused && !incomplete)
            || tracked.last_task_id.as_deref() != Some(&task.id)
            || source.input.commit != *expected
        {
            return Ok(false);
        }
        // Plans precede writes, including writes whose receipts may have been lost. A retry
        // request alone carries no remote effect and must not strand an obsolete assessment.
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
        if (tracked.paused && last_pause > last_retry)
            || events.iter().any(|event| {
                (event.kind.starts_with("publication.")
                    && event.kind != "publication.retry_requested")
                    || event.kind.starts_with("assessment.")
            })
            || !self.db.list_merge_requests(&task.id)?.is_empty()
        {
            return Ok(false);
        }
        self.retire_superseded_source(task, &attempt.id, tracked, expected, actual, None)?;
        Ok(true)
    }

    /// Close an obsolete source task and let discovery assess the new head after settling, within
    /// the source's existing run budget. Callers must first rule out unknown remote write outcomes.
    pub(crate) fn retire_superseded_source(
        &mut self,
        task: &Task,
        attempt_id: &str,
        mut tracked: TrackedPullRequest,
        expected: &CommitSha,
        actual: &CommitSha,
        cause: Option<&str>,
    ) -> Result<()> {
        let discovery = self.config.projects[&task.project]
            .pull_requests
            .as_ref()
            .expect("checked authority");
        let now = Utc::now();
        if tracked.observed_head != actual.0 {
            tracked.observed_head = actual.0.clone();
            tracked.head_observed_at = now;
        }
        if tracked.runs >= discovery.max_runs_per_pull_request {
            tracked.paused = true;
            tracked.follow_up = None;
            tracked.status = "Automatic evaluation budget reached after source head changed; explicit resume required".into();
        } else {
            tracked.paused = false;
            tracked.status =
                "Source head changed before delivery; waiting for a fresh evaluation".into();
            tracked.follow_up = Some(PullRequestFollowUp {
                at: (tracked.head_observed_at
                    + Duration::seconds(i64::from(discovery.settle_seconds)))
                .max(now),
                reason: PullRequestFollowUpReason::Settling,
            });
        }
        let mut payload = serde_json::json!({"expected_head":expected,"observed_head":actual,"runs":tracked.runs,"paused":tracked.paused});
        if let Some(cause) = cause {
            payload["cause"] = cause.into();
        }
        self.db.record_event(
            Some(&task.id),
            Some(attempt_id),
            "orchestrator",
            "assessment.superseded",
            payload,
        )?;
        self.db
            .set_task_state(&task.id, TaskState::Closed, "orchestrator")?;
        self.db.save_pull_request_source(&tracked)?;
        Ok(())
    }
}
