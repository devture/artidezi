// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Retire a result for an obsolete source head, closing this task's own published adaptations
//! first. Never replay that result or reset its source budget; discovery must freeze and assess
//! the new revision before any publication.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_contract::{
    common::{CommitSha, Outcome},
    result::AgentResult,
};
use artidezi_forge::{ExpectedProposal, MergeRequestState as RemoteState};
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

    /// Close published adaptations of a moved source and retire their task,
    /// unless an operator paused it.
    pub(crate) fn retire_superseded_adaptations(&mut self, task: &Task) -> Result<bool> {
        let Some(source) = crate::discovery::source_of(task)? else {
            return Ok(false);
        };
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let events = self.db.list_events(&task.id)?;
        let last = |kind: &str| {
            events
                .iter()
                .filter(|event| event.kind == kind)
                .map(|event| event.id)
                .max()
                .unwrap_or(0)
        };
        if tracked.paused && last("discovery.paused") > last("publication.retry_requested") {
            return Ok(false);
        }
        let Some((attempt_id, expected, actual)) = self.close_superseded_adaptations(task)? else {
            return Ok(false);
        };
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        self.retire_superseded_source(
            task,
            &attempt_id,
            tracked,
            &expected,
            &actual,
            Some("published_adaptation"),
        )?;
        Ok(true)
    }

    /// Close this task's own adaptations once the open source moved past their head,
    /// touching nothing unless every proposal is provably ours.
    pub(crate) fn close_superseded_adaptations(
        &mut self,
        task: &Task,
    ) -> Result<Option<(String, CommitSha, CommitSha)>> {
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let Some(source) = policy.source_pull_request.as_ref() else {
            return Ok(None);
        };
        if self.require_source_authority(task, &policy).is_err() {
            return Ok(None);
        }
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(None);
        };
        let proposals = self.db.list_merge_requests(&task.id)?;
        if tracked.last_task_id.as_deref() != Some(&task.id)
            || !attempt.state.is_terminal()
            || proposals.is_empty()
        {
            return Ok(None);
        }
        let repo = &policy.repositories[&source.repository_id];
        let current = self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, source.number)?;
        if current.state != RemoteState::Open
            || current.head_commit == source.input.commit
            || current.source_branch != source.source_branch
            || current.target_branch != source.target_branch
            || current.author_login != source.author
        {
            return Ok(None);
        }
        let events = self.db.list_events(&task.id)?;
        let recorded = |repository: &serde_json::Value, branch: &serde_json::Value| {
            proposals.iter().any(|mr| {
                *repository == mr.repository_id.as_str() && *branch == mr.source_branch.as_str()
            })
        };
        // A plan without a receipt may stand for a proposal whose creation response was lost.
        if events
            .iter()
            .filter(|event| event.kind == "publication.planned")
            .any(|plan| !recorded(&plan.payload["repository"], &plan.payload["branch"]))
        {
            return Ok(None);
        }
        let close_planned = |mr: &PublishedMergeRequest| {
            events.iter().any(|event| {
                event.kind == "merge_request.close_planned"
                    && event.payload["repository"] == mr.repository_id.as_str()
                    && event.payload["number"] == mr.number
            })
        };
        let mut closing = Vec::new();
        for mr in proposals {
            let Some(repo) = policy.repositories.get(&mr.repository_id) else {
                return Ok(None);
            };
            let info = self
                .forge(&repo.forge)?
                .get_merge_request(&repo.remote, mr.number)?;
            let ours = mr.forge == repo.forge
                && mr.source_branch == format!("artidezi/{}", task.id)
                && info.author_login == self.config.forges[&repo.forge].bot_login
                && info.source_branch == mr.source_branch
                && info.target_branch == repo.branch
                && info.head_commit.as_str() == mr.head_commit;
            match info.state {
                RemoteState::Open if ours => closing.push((mr, true)),
                RemoteState::Closed if ours && close_planned(&mr) => closing.push((mr, false)),
                _ => return Ok(None),
            }
        }
        let comment = format!(
            "Superseded by {} at `{}`; this adaptation was prepared for `{}`.",
            source.url, current.head_commit, source.input.commit
        );
        for (mut mr, open) in closing {
            let repo = &policy.repositories[&mr.repository_id];
            if open {
                self.db.record_event(
                    Some(&task.id),
                    Some(&attempt.id),
                    "orchestrator",
                    "merge_request.close_planned",
                    serde_json::json!({"repository":mr.repository_id,"number":mr.number,"source_head":current.head_commit}),
                )?;
                // A retry after a lost response can repeat the comment, but never the close.
                let head = CommitSha(mr.head_commit.clone());
                let expected = ExpectedProposal {
                    number: mr.number,
                    source_branch: &mr.source_branch,
                    target_branch: &repo.branch,
                    head: &head,
                };
                self.forge(&repo.forge)?
                    .close_merge_request(&repo.remote, &expected, &comment)?;
            }
            if mr.state == MergeRequestState::Closed {
                continue;
            }
            mr.state = MergeRequestState::Closed;
            mr.updated_at = Utc::now();
            self.db.upsert_merge_request(&mr)?;
            self.db.record_event(
                Some(&task.id),
                Some(&attempt.id),
                "orchestrator",
                "merge_request.superseded",
                serde_json::json!({"repository":mr.repository_id,"number":mr.number,"url":mr.url,"head":mr.head_commit,"source_head":current.head_commit}),
            )?;
        }
        Ok(Some((
            attempt.id,
            source.input.commit.clone(),
            current.head_commit,
        )))
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
        self.db
            .supersede_source_task(&task.id, attempt_id, "orchestrator", payload, &tracked)?;
        Ok(())
    }
}
