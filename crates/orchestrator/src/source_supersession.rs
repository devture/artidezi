// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Preserve published adaptations across proven source rebases; otherwise retire obsolete work.
//! Equivalence never authorizes publication or replays a result against a different source head.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_contract::{
    common::{CommitSha, Outcome},
    result::AgentResult,
};
use artidezi_forge::{ExpectedProposal, MergeRequestInfo, MergeRequestState as RemoteState};
use artidezi_store::model::*;
use chrono::{Duration, Utc};

use crate::{Orchestrator, OrchestratorError, Result};

impl Orchestrator {
    /// Durable equivalence applies only to delivered adaptations, never source reviews or dry runs.
    pub(crate) fn equivalent_adaptation_head(&self, task: &Task, head: &CommitSha) -> Result<bool> {
        let Some(source) = crate::discovery::source_of(task)? else {
            return Ok(false);
        };
        Ok(!self.db.list_merge_requests(&task.id)?.is_empty()
            && self.db.list_events(&task.id)?.iter().any(|event| {
                event.kind == "source.equivalent_head_observed"
                    && event.payload["task"] == task.id
                    && event.payload["frozen_head"] == source.input.commit.as_str()
                    && event.payload["observed_head"] == head.as_str()
            }))
    }

    /// Read-only Git proof, followed by local evidence. Callers still verify proposal ownership
    /// and state. Any unavailable or ambiguous Git evidence falls back to normal supersession.
    pub(crate) fn preserve_equivalent_adaptation(
        &mut self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        current: &MergeRequestInfo,
    ) -> Result<bool> {
        let Some(source) = policy.source_pull_request.as_ref() else {
            return Ok(false);
        };
        if current.head_commit == source.input.commit
            || current.state != RemoteState::Open
            || current.source_branch != source.source_branch
            || current.target_branch != source.target_branch
            || current.author_login != source.author
            || current.draft
            || source
                .title
                .as_ref()
                .is_some_and(|title| *title != current.title)
        {
            return Ok(false);
        }
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let proposals = self.db.list_merge_requests(&task.id)?;
        let events = self.db.list_events(&task.id)?;
        if tracked.last_task_id.as_deref() != Some(&task.id)
            || proposals.is_empty()
            || proposals
                .iter()
                .any(|mr| mr.state == MergeRequestState::Closed)
            || events.iter().any(|event| {
                event.kind == "merge_request.close_planned"
                    || (event.kind == "publication.planned"
                        && !proposals.iter().any(|mr| {
                            event.payload["repository"] == mr.repository_id
                                && event.payload["branch"] == mr.source_branch
                        }))
            })
        {
            return Ok(false);
        }
        if self.equivalent_adaptation_head(task, &current.head_commit)? {
            return Ok(true);
        }
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(false);
        };
        let job = self.load_job(&attempt)?;
        let Some(frozen) = job
            .repositories
            .iter()
            .find(|r| r.id.as_str() == source.repository_id)
        else {
            return Ok(false);
        };
        let repo = &policy.repositories[&source.repository_id];
        let proof = (|| -> Result<Option<CommitSha>> {
            let forge = self.forge(&repo.forge)?;
            let fresh_target = forge.branch_head(&repo.remote, &repo.branch)?;
            let remote_ref = forge.merge_request_ref(&repo.remote, source.number)?;
            let cache = self.cache(&repo.forge, &repo.remote)?;
            let target_ref = format!("refs/heads/{}", repo.branch);
            let source_ref = format!("refs/artidezi/discovery/{}", source.tracking_id);
            cache.fetch(
                &forge.clone_url(&repo.remote),
                &[
                    &format!("+{target_ref}:{target_ref}"),
                    &format!("+{remote_ref}:{source_ref}"),
                ],
                forge.git_credentials(&repo.remote)?.as_ref(),
            )?;
            let target = git2::Oid::from_str(fresh_target.as_str())?;
            let head = git2::Oid::from_str(current.head_commit.as_str())?;
            if cache.resolve(&target_ref)? != target || cache.resolve(&source_ref)? != head {
                return Ok(None);
            }
            Ok(artidezi_git::equivalence::same_change(
                cache.repo(),
                (
                    git2::Oid::from_str(frozen.base_commit.as_str())?,
                    git2::Oid::from_str(source.input.commit.as_str())?,
                ),
                (target, head),
            )?
            .then_some(fresh_target))
        })();
        let fresh_target = match proof {
            Ok(Some(target)) => target,
            Ok(None) => return Ok(false),
            Err(error) => {
                tracing::debug!(task = %task.id, %error, "source equivalence not proven");
                return Ok(false);
            }
        };
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "source.equivalent_head_observed",
            serde_json::json!({"task":task.id,"frozen_head":source.input.commit,"observed_head":current.head_commit,"frozen_target":frozen.base_commit,"fresh_target":fresh_target}),
        )?;
        Ok(true)
    }

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
        // Even an equivalent rebase does not authorize publishing an unpublished verdict for a
        // different commit. Keep the normal fresh-assessment and bounded transient-retry gates.
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
        let mut merged = false;
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
                RemoteState::Merged if ours => merged = true,
                _ => return Ok(None),
            }
        }
        if self.preserve_equivalent_adaptation(task, &policy, &current)? {
            return Ok(None);
        }
        if merged {
            return Ok(None);
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
            if mr.state != MergeRequestState::Closed {
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
            self.cleanup_superseded_branch(task, &attempt.id, &policy, &mr);
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
