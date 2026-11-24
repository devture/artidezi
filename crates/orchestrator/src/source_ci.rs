// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Resume only explicitly deferred, validated source approvals/merges. Other publication failures
//! remain manual recovery. Waiting occupies no worker; an unpublished superseded result may
//! retire into discovery for a fresh assessment under the existing source budget.
//!
//! Failed checks hold delivery for a human, but transient CI failures are common (a forge outage,
//! a flaky runner). The hold is rechecked on the same cadence: green checks on the assessed head
//! retry delivery through every publication gate, and a new head retires into discovery.

use artidezi_config::{ResolvedProjectPolicy, model::SafeVerdictAction};
use artidezi_contract::time::{SignedDuration, Timestamp};
use artidezi_contract::{attempt::AttemptRecord, common::Outcome, result::AgentResult};
use artidezi_forge::{ChecksState, MergeRequestState as RemoteState};
use artidezi_store::model::*;

use crate::{
    Orchestrator, OrchestratorError, Result,
    discovery::{FOLLOW_UP_INTERVAL_SECONDS, source_of},
};

impl Orchestrator {
    fn source_ci_interval(&self, _task: &Task) -> SignedDuration {
        // Delivery checks have their own cadence, independent of repository-wide discovery.
        SignedDuration::from_secs(FOLLOW_UP_INTERVAL_SECONDS)
    }

    pub(crate) fn wait_for_source_ci(&mut self, task: &Task) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        self.db
            .wait_for_source_ci(&task.id, Timestamp::now() + self.source_ci_interval(task))?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        tracked.status = "Waiting for CI; delivery will continue automatically".into();
        self.db.save_pull_request_source(&tracked)?;
        Ok(())
    }

    pub(crate) fn resume_source_ci(&mut self, task: &Task, now: Timestamp) -> Result<TaskState> {
        if task.state != TaskState::WaitingForCi
            || !self
                .db
                .claim_source_ci_check(&task.id, now, now + self.source_ci_interval(task))?
        {
            return Ok(task.state);
        }
        let attempt = self
            .db
            .latest_attempt(&task.id)?
            .ok_or_else(|| OrchestratorError::Invalid("missing waiting attempt".into()))?;
        if let Err(error) = self.resume_source_ci_inner(task, &attempt) {
            if self.reschedule_superseded_source(task, &attempt, &error)? {
                self.persist_forge_backoffs()?;
                return Ok(self.db.get_task(&task.id)?.state);
            }
            match error {
                OrchestratorError::ChecksFailed => self.hold_for_failed_checks(task, &attempt)?,
                OrchestratorError::WaitingForCi => {}
                // Only preflight reads reach here. Respect persisted forge backoff and try on a
                // later scheduled check; never convert an API error into successful CI.
                ref error
                    if error.is_temporary()
                        || matches!(
                            error,
                            OrchestratorError::Forge(artidezi_forge::ForgeError::RetryAfter { .. })
                        ) =>
                {
                    if let Some(source) = source_of(task)? {
                        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
                        tracked.status = format!("Waiting for CI; checking it failed: {error}");
                        self.db.save_pull_request_source(&tracked)?;
                    }
                }
                OrchestratorError::SourceHeadChanged { .. }
                    if self.reassess_moved_source(task, &attempt)? => {}
                _ => self.stop_delivery(
                    task,
                    &attempt,
                    &error,
                    &format!("Automatic delivery stopped: {error}"),
                )?,
            }
        }
        self.persist_forge_backoffs()?;
        Ok(self.db.get_task(&task.id)?.state)
    }

    fn resume_source_ci_inner(&mut self, task: &Task, attempt: &Attempt) -> Result<()> {
        if attempt.state != AttemptState::Valid {
            return Err(OrchestratorError::Invalid(
                "waiting requires a validated result".into(),
            ));
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        self.require_source_authority(task, &policy)?;
        let source = policy
            .source_pull_request
            .as_ref()
            .expect("checked authority");
        let result: AgentResult = self.artifacts.get_json(
            attempt
                .result
                .as_ref()
                .ok_or_else(|| OrchestratorError::Invalid("missing accepted result".into()))?,
        )?;
        if source.safe_verdict_action == SafeVerdictAction::Comment
            || result.outcome != Outcome::ApproveUntouched
        {
            return Err(OrchestratorError::Invalid(
                "only unchanged approvals/merges may wait for CI".into(),
            ));
        }
        if self.recover_completed_source_action(task, attempt, &policy)? {
            return Ok(());
        }
        self.require_source_action_ready(task, &policy)?;
        let record: AttemptRecord =
            self.artifacts
                .get_json(attempt.attempt_record.as_ref().ok_or_else(|| {
                    OrchestratorError::Invalid("missing accepted attempt record".into())
                })?)?;
        let job = self.load_job(attempt)?;
        // A crash from here onward follows existing explicit publication recovery. A waiting state
        // is not permission to blindly repeat a write with an unknown outcome.
        self.db
            .set_task_state(&task.id, TaskState::Working, "orchestrator")?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "publication.ci_passed",
            serde_json::json!({}),
        )?;
        let mut publishing = task.clone();
        publishing.state = TaskState::Working;
        if let Err(error) = self.publish_source_result(&publishing, attempt, &job, &record, &result)
        {
            self.delivery_failed(
                &publishing,
                attempt,
                &error,
                &format!("Automatic publication interrupted: {error}"),
            )?;
        }
        Ok(())
    }

    /// Hold automatic delivery after failed checks on the assessed head, rechecking it on the same
    /// cadence. An approval or merge with a lost response does not need a person: redelivery looks
    /// it up, and both are bound to this head.
    pub(crate) fn hold_for_failed_checks(&mut self, task: &Task, attempt: &Attempt) -> Result<()> {
        let reason = OrchestratorError::ChecksFailed.to_string();
        let mut hold = self.new_hold(task, HoldReason::FailedChecks, reason.as_str())?;
        hold.releases = vec![HoldRelease::GreenChecks, HoldRelease::NewHead];
        hold.next_at = Some(Timestamp::now() + self.source_ci_interval(task));
        let payload = serde_json::json!({"reason": reason, "hold": hold.reason, "head": hold.head});
        self.record_hold(
            task,
            attempt,
            TaskState::NeedsHuman,
            "task.needs_human",
            payload,
            &hold,
        )
    }

    /// Recheck a due hold recorded by [`Self::hold_for_failed_checks`]. A later hold or explicit
    /// pause replaces it, which leaves recovery to a person.
    pub(crate) fn recheck_failed_source_checks(
        &mut self,
        task: &Task,
        now: Timestamp,
    ) -> Result<()> {
        // Reserve the next check before contacting the forge, so restarts cannot hot-loop.
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let Some(hold) = &mut tracked.hold else {
            return Ok(());
        };
        hold.next_at = Some(now + self.source_ci_interval(task));
        self.db.save_pull_request_source(&tracked)?;
        let attempt = self
            .db
            .latest_attempt(&task.id)?
            .ok_or_else(|| OrchestratorError::Invalid("missing held attempt".into()))?;
        match self.recheck_failed_source_checks_inner(task, &attempt) {
            Ok(()) => Ok(()),
            // Keep the hold and its schedule; a later check sees the current forge state.
            Err(OrchestratorError::Forge(_) | OrchestratorError::Store(_)) => Ok(()),
            // It closed with its change, which was reopened since; its hold still says why.
            Err(error) if task.state == TaskState::Closed => {
                tracing::debug!(task = %task.id, %error, "reopened change not evaluated again yet");
                Ok(())
            }
            Err(error) => self.stop_delivery(
                task,
                &attempt,
                &error,
                &format!("Automatic check recovery stopped: {error}"),
            ),
        }
    }

    fn recheck_failed_source_checks_inner(&mut self, task: &Task, attempt: &Attempt) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if !tracked.paused || tracked.last_task_id.as_deref() != Some(&task.id) {
            return Ok(());
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        self.require_source_authority(task, &policy)?;
        let repo = &policy.repositories[&source.repository_id];
        let current = self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, source.number)?;
        if current.state != RemoteState::Open {
            // Discovery reconciles closed and merged sources.
            return Ok(());
        }
        // A task that closed with its change cannot deliver after a reopening; a new one can.
        let closed = task.state == TaskState::Closed;
        let moved = current.head_commit != source.input.commit;
        if closed && !moved && current.checks != ChecksState::Success {
            return Ok(());
        }
        if closed || moved {
            // Equivalence preserves published adaptations only; it cannot reuse a source
            // approval or authorize a merge of this different commit.
            // A maintainer objection made during the hold is a human decision; a new head alone
            // must not reactivate automatic evaluation past it.
            let mut seen = tracked.clone();
            seen.observed_head = current.head_commit.0.clone();
            if self.person_objects(task, &source, &seen)? {
                return Err(OrchestratorError::Outdated(
                    "a reviewer has requested changes, or the published assessment was edited or removed; maintainer input required".into(),
                ));
            }
            return self.retire_superseded_source(
                task,
                &attempt.id,
                tracked,
                &source.input.commit,
                &current.head_commit,
                Some("failed_checks"),
                None,
            );
        }
        if current.checks != ChecksState::Success {
            return Ok(());
        }
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "delivery.checks_recovered",
            serde_json::json!({"head": source.input.commit}),
        )?;
        // Every publication gate runs again, including reviews and freshness. Failures there
        // record their own hold; failed checks again record a new recheckable hold.
        if let Err(error) = self.retry_publication(&task.id, "orchestrator") {
            tracing::warn!(task = %task.id, %error, "delivery after recovered checks withheld");
        }
        Ok(())
    }
}
