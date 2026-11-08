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
use artidezi_contract::{attempt::AttemptRecord, common::Outcome, result::AgentResult};
use artidezi_forge::{ChecksState, MergeRequestState as RemoteState};
use artidezi_store::model::*;
use chrono::{DateTime, Duration, Utc};

use crate::{
    Orchestrator, OrchestratorError, Result,
    discovery::{FOLLOW_UP_INTERVAL_SECONDS, source_of},
};

impl Orchestrator {
    /// Marks the `task.needs_human` events that [`Self::recheck_failed_source_checks`] may release.
    const FAILED_CHECKS_HOLD: &str = "failed_checks";

    fn source_ci_interval(&self, _task: &Task) -> Duration {
        // Delivery checks have their own cadence, independent of repository-wide discovery.
        Duration::seconds(FOLLOW_UP_INTERVAL_SECONDS)
    }

    pub(crate) fn wait_for_source_ci(&mut self, task: &Task) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        self.db
            .wait_for_source_ci(&task.id, Utc::now() + self.source_ci_interval(task))?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        tracked.status = "Waiting for CI; delivery will continue automatically".into();
        self.db.save_pull_request_source(&tracked)?;
        Ok(())
    }

    pub(crate) fn resume_source_ci(
        &mut self,
        task: &Task,
        now: DateTime<Utc>,
    ) -> Result<TaskState> {
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
                OrchestratorError::WaitingForCi
                | OrchestratorError::Forge(artidezi_forge::ForgeError::RetryAfter { .. }) => {
                    // Only preflight reads reach here. Respect persisted forge backoff and try on
                    // a later scheduled check; never convert an API error into successful CI.
                }
                _ => self.needs_human(
                    task,
                    &attempt,
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
            self.needs_human(
                &publishing,
                attempt,
                &format!("Automatic publication interrupted: {error}"),
            )?;
        }
        Ok(())
    }

    /// Hold automatic delivery after failed checks on the assessed head. The hold is recheckable
    /// only when no earlier source write has an unconfirmed outcome: the checks gate runs before an
    /// approval is planned and after it is published, but an explicit retry also reaches it after
    /// a lost approval or merge response, which must stay with a person.
    pub(crate) fn hold_for_failed_checks(&mut self, task: &Task, attempt: &Attempt) -> Result<()> {
        let reason = OrchestratorError::ChecksFailed.to_string();
        let events = self.db.list_events(&task.id)?;
        let unconfirmed = events
            .iter()
            .any(|event| event.kind == "source.merge_planned")
            || (events
                .iter()
                .any(|event| event.kind == "assessment.approval_planned")
                && !events
                    .iter()
                    .any(|event| event.kind == "assessment.published"));
        let current = self.db.get_task(&task.id)?;
        if unconfirmed || current.state.is_terminal() {
            return self.needs_human(
                task,
                attempt,
                &format!(
                    "{reason}. An earlier approval or merge is unconfirmed; inspect the source"
                ),
            );
        }
        let head = source_of(task)?.map(|source| source.input.commit);
        self.pause_task_source(task, &reason)?;
        self.db.hold_for_source_check(
            &task.id,
            &attempt.id,
            "orchestrator",
            serde_json::json!({"reason": reason, "recheck": Self::FAILED_CHECKS_HOLD, "head": head}),
            Utc::now() + self.source_ci_interval(task),
        )?;
        Ok(())
    }

    /// Release only the latest hold recorded by [`Self::hold_for_failed_checks`]. A later hold, an
    /// explicit pause, a human resume or a newer evaluation leaves recovery to a person.
    pub(crate) fn recheck_failed_source_checks(
        &mut self,
        task: &Task,
        now: DateTime<Utc>,
    ) -> Result<TaskState> {
        if task.state != TaskState::NeedsHuman {
            return Ok(task.state);
        }
        let events = self.db.list_events(&task.id)?;
        let Some(hold) = events
            .iter()
            .rev()
            .find(|event| event.kind == "task.needs_human")
        else {
            return Ok(task.state);
        };
        if hold.payload["recheck"] != Self::FAILED_CHECKS_HOLD
            || events
                .iter()
                .any(|event| event.id > hold.id && event.kind == "discovery.paused")
            || !self.db.claim_held_source_check(
                &task.id,
                now,
                now + self.source_ci_interval(task),
            )?
        {
            return Ok(task.state);
        }
        let attempt = self
            .db
            .latest_attempt(&task.id)?
            .ok_or_else(|| OrchestratorError::Invalid("missing held attempt".into()))?;
        match self.recheck_failed_source_checks_inner(task, &attempt) {
            Ok(()) => {}
            Err(OrchestratorError::Forge(_) | OrchestratorError::Store(_)) => {
                // Keep the hold and its schedule; a later check sees the current forge state.
            }
            Err(error) => self.needs_human(
                task,
                &attempt,
                &format!("Automatic check recovery stopped: {error}"),
            )?,
        }
        self.persist_forge_backoffs()?;
        Ok(self.db.get_task(&task.id)?.state)
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
        if current.head_commit != source.input.commit {
            // A maintainer objection made during the hold is a human decision; a new head alone
            // must not reactivate automatic evaluation past it.
            if self.source_changes_requested(&policy)? {
                return Err(OrchestratorError::Invalid(
                    "a reviewer has requested changes; maintainer input required".into(),
                ));
            }
            return self.retire_superseded_source(
                task,
                &attempt.id,
                tracked,
                &source.input.commit,
                &current.head_commit,
                Some(Self::FAILED_CHECKS_HOLD),
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
