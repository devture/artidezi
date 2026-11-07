// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Resume only explicitly deferred, validated source approvals/merges. Other publication failures
//! remain manual recovery. Waiting occupies no worker; an unpublished superseded result may
//! retire into discovery for a fresh assessment under the existing source budget.

use artidezi_config::{ResolvedProjectPolicy, model::SafeVerdictAction};
use artidezi_contract::{attempt::AttemptRecord, common::Outcome, result::AgentResult};
use artidezi_store::model::*;
use chrono::{DateTime, Duration, Utc};

use crate::{
    Orchestrator, OrchestratorError, Result,
    discovery::{FOLLOW_UP_INTERVAL_SECONDS, source_of},
};

impl Orchestrator {
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
}
