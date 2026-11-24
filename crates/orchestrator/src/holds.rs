// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Holds: why a change or task is stuck, and what moves it on again.
//!
//! [`Orchestrator::process_holds`] is the one place that acts on a hold's schedule. A delivery hold
//! repeats delivery of an already validated result, without another agent run. Every retry goes
//! through the same gates as the first delivery and first looks for what already happened.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::{DeliveryMode, DeliveryRetries};
use artidezi_contract::common::CommitSha;
use artidezi_contract::time::{SignedDuration, Timestamp};
use artidezi_contract::{attempt::AttemptRecord, result::AgentResult};
use artidezi_forge::{ForgeError, MergeRequestState as RemoteState};
use artidezi_store::model::*;

use crate::{Orchestrator, OrchestratorError, Result, discovery::source_of};

pub(crate) const RETRY_SCHEDULED: &str = "delivery.retry_scheduled";
pub(crate) const RETRY_STARTED: &str = "delivery.retry_started";
/// Actors of `publication.retry_requested` that are not people.
const AUTOMATIC_ACTORS: [&str; 2] = ["orchestrator", "recovery"];

impl Orchestrator {
    /// A hold on `task`, carrying what its assessment was based on.
    pub(crate) fn new_hold(
        &self,
        task: &Task,
        reason: HoldReason,
        message: impl Into<String>,
    ) -> Result<Hold> {
        let mut hold = Hold::new(reason, message);
        hold.task_id = Some(task.id.clone());
        if let Some(source) = source_of(task)? {
            hold.target = task
                .repositories
                .iter()
                .find(|repository| repository.repository_id == source.repository_id)
                .map(|repository| repository.base_commit.clone());
            hold.head = Some(source.input.commit.0);
        }
        Ok(hold)
    }

    /// Hold `task` for a person.
    pub(crate) fn hold_for_person(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        reason: HoldReason,
        message: &str,
    ) -> Result<()> {
        let hold = self.new_hold(task, reason, message)?;
        self.record_hold(
            task,
            attempt,
            TaskState::NeedsHuman,
            "task.needs_human",
            serde_json::json!({"reason": message, "hold": reason}),
            &hold,
        )
    }

    pub(crate) fn record_hold(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        state: TaskState,
        event: &str,
        payload: serde_json::Value,
        hold: &Hold,
    ) -> Result<()> {
        let source = match source_of(task)? {
            Some(source) => {
                let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
                tracked.place_hold(hold.clone());
                Some(tracked)
            }
            None => None,
        };
        let task_hold = (source.is_none() && state == TaskState::WaitingToRetry).then_some(hold);
        self.db.hold_task(
            TaskHold {
                task_id: &task.id,
                attempt_id: &attempt.id,
                state,
                event,
                payload,
                source: source.as_ref(),
                hold: task_hold,
            },
            "orchestrator",
        )?;
        Ok(())
    }

    /// Limits and delays for delivering results of `task`, from the current configuration.
    pub(crate) fn delivery_retries(&self, task: &Task) -> DeliveryRetries {
        self.config
            .projects
            .get(&task.project)
            .and_then(|project| project.pull_requests.as_ref())
            .map_or_else(DeliveryRetries::default, |discovery| {
                discovery.retries.delivery
            })
    }

    /// Delivery retries scheduled for this result since a person last asked for one.
    pub(crate) fn delivery_retries_used(&self, task: &Task, attempt: &Attempt) -> Result<u32> {
        let events = self.db.list_events(&task.id)?;
        let since = events
            .iter()
            .filter(|event| {
                event.kind == "publication.retry_requested"
                    && !AUTOMATIC_ACTORS.contains(&event.actor.as_str())
            })
            .map(|event| event.id)
            .max()
            .unwrap_or(0);
        Ok(events
            .iter()
            .filter(|event| {
                event.kind == RETRY_SCHEDULED
                    && event.id > since
                    && event.attempt_id.as_deref() == Some(&attempt.id)
            })
            .count() as u32)
    }

    /// Whether delivery of a validated result of `task` may be repeated: every effect is planned
    /// durably, bound to an exact commit and looked up before it is repeated. Direct pushes are not.
    fn delivery_repeatable(&self, task: &Task) -> bool {
        match task.kind {
            TaskKind::PullRequest => true,
            TaskKind::Manual => matches!(
                task.delivery_mode,
                DeliveryMode::MergeRequest | DeliveryMode::MergeRequestWithAutomerge
            ),
            TaskKind::Renovate => false,
        }
    }

    /// Delivery of a validated result failed: retry it later if only time is needed, else ask.
    pub(crate) fn delivery_failed(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        error: &OrchestratorError,
        message: &str,
    ) -> Result<()> {
        let mut message = message.to_owned();
        if error.is_temporary() {
            let not_before = match error {
                OrchestratorError::Forge(ForgeError::RetryAfter { retry_at, .. }) => {
                    Some(*retry_at)
                }
                _ => None,
            };
            if self.schedule_delivery_retry(task, attempt, &error.to_string(), not_before)? {
                return Ok(());
            }
            let used = self.delivery_retries_used(task, attempt)?;
            if used > 0 {
                message = format!("{message} (still failing after {used} automatic retries)");
            }
        }
        self.needs_human(task, attempt, &message)
    }

    /// Returns false when this delivery cannot or may no longer be repeated automatically.
    pub(crate) fn schedule_delivery_retry(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        cause: &str,
        not_before: Option<Timestamp>,
    ) -> Result<bool> {
        let task = self.db.get_task(&task.id)?;
        let attempt = self.db.get_attempt(&attempt.id)?;
        if attempt.state != AttemptState::Valid
            || !task.state.can_transition_to(TaskState::WaitingToRetry)
            || !self.delivery_repeatable(&task)
        {
            return Ok(false);
        }
        if let Some(source) = source_of(&task)? {
            let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
            if tracked.paused || tracked.last_task_id.as_deref() != Some(&task.id) {
                return Ok(false);
            }
        }
        let limits = self.delivery_retries(&task);
        let used = self.delivery_retries_used(&task, &attempt)?;
        if used >= limits.max {
            return Ok(false);
        }
        let cause = format!("{cause}{}", self.delivery_stage(&task)?);
        let now = Timestamp::now();
        let at = now + SignedDuration::from_secs(i64::from(limits.delay_seconds(used)));
        let mut hold = self.new_hold(&task, HoldReason::Delivery, cause.as_str())?;
        hold.releases = vec![HoldRelease::Time];
        hold.retries = used + 1;
        hold.max_retries = limits.max;
        hold.next_at = Some(not_before.map_or(at, |wait| wait.max(at)));
        self.record_hold(
            &task,
            &attempt,
            TaskState::WaitingToRetry,
            RETRY_SCHEDULED,
            serde_json::json!({
                "reason": cause,
                "retry": hold.retries,
                "max_retries": hold.max_retries,
                "at": hold.next_at,
            }),
            &hold,
        )?;
        Ok(true)
    }

    /// What an interrupted delivery was doing, from the last effect it planned.
    fn delivery_stage(&self, task: &Task) -> Result<&'static str> {
        Ok(self
            .db
            .list_events(&task.id)?
            .iter()
            .rev()
            .find_map(|event| match event.kind.as_str() {
                "source.merge_planned" => Some(" while merging"),
                "assessment.approval_planned" => Some(" while approving"),
                "publication.planned" | "manual_publication.planned" => {
                    Some(" while publishing the proposal")
                }
                "assessment.published"
                | "merge_request.published"
                | "source.merge_accepted"
                | "attempt.created"
                | RETRY_STARTED => Some(""),
                _ => None,
            })
            .unwrap_or_default())
    }

    /// Act on every hold whose time has come. Holds only a person can release are left alone.
    pub(crate) fn process_holds(&mut self, now: Timestamp) -> Result<HoldsReport> {
        let mut due: Vec<(String, HoldReason)> = Vec::new();
        for source in self.db.list_pull_request_sources(None)? {
            if let Some(hold) = &source.hold
                && hold.due(now)
                && let Some(task_id) = &hold.task_id
                && source.last_task_id.as_ref() == Some(task_id)
            {
                due.push((task_id.clone(), hold.reason));
            }
        }
        for (task_id, hold) in self.db.list_task_holds()? {
            if hold.due(now) {
                due.push((task_id, hold.reason));
            }
        }
        let mut report = HoldsReport::default();
        for (task_id, reason) in due {
            let task = self.db.get_task(&task_id)?;
            let result = match (reason, task.state) {
                (HoldReason::Delivery, TaskState::WaitingToRetry) => self.retry_delivery(&task),
                (HoldReason::FailedChecks, TaskState::NeedsHuman) => {
                    self.recheck_failed_source_checks(&task, now)
                }
                _ => continue,
            };
            let backoffs = self.persist_forge_backoffs();
            match result.and(backoffs) {
                Ok(()) => {
                    let state = self.db.get_task(&task_id)?.state;
                    if state != task.state {
                        report.moved.push((task_id, state));
                    }
                }
                Err(error) => report.errors.push(format!("{task_id}: {error}")),
            }
        }
        Ok(report)
    }

    /// Deliver the validated result again. The task leaves its waiting state first: a crash from
    /// here on is an interrupted delivery, which startup schedules as the next retry.
    fn retry_delivery(&mut self, task: &Task) -> Result<()> {
        let attempt = self
            .db
            .latest_attempt(&task.id)?
            .ok_or_else(|| OrchestratorError::Invalid("missing attempt to deliver".into()))?;
        self.db
            .set_task_state(&task.id, TaskState::Working, "orchestrator")?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            RETRY_STARTED,
            serde_json::json!({"retry": self.delivery_retries_used(task, &attempt)?}),
        )?;
        let task = self.db.get_task(&task.id)?;
        if let Err(error) = self.redeliver(&task, &attempt) {
            let moved = matches!(
                error,
                OrchestratorError::SourceHeadChanged { .. }
                    | OrchestratorError::Forge(ForgeError::HeadMoved { .. })
            );
            if !(moved && self.reassess_moved_source(&task, &attempt)?)
                && !self.close_gone_source(&task, &error)?
            {
                self.delivery_failed(
                    &task,
                    &attempt,
                    &error,
                    &format!("automatic delivery retry stopped: {error}"),
                )?;
            }
        }
        self.clear_delivery_hold(&task)
    }

    fn redeliver(&mut self, task: &Task, attempt: &Attempt) -> Result<()> {
        let missing = |what: &str| OrchestratorError::Invalid(format!("missing {what}"));
        if attempt.state != AttemptState::Valid {
            return Err(OrchestratorError::Invalid(
                "delivery requires a validated result".into(),
            ));
        }
        let record: AttemptRecord = self.artifacts.get_json(
            attempt
                .attempt_record
                .as_ref()
                .ok_or_else(|| missing("attempt record"))?,
        )?;
        let result: AgentResult = self
            .artifacts
            .get_json(attempt.result.as_ref().ok_or_else(|| missing("result"))?)?;
        let job = self.load_job(attempt)?;
        self.apply_outcome(task, attempt, &job, &record, &result)
    }

    /// A delivered, superseded or stopped task no longer retries; drop the hold that said so.
    pub(crate) fn clear_delivery_hold(&mut self, task: &Task) -> Result<()> {
        let Some(source) = source_of(task)? else {
            return Ok(());
        };
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if self.db.get_task(&task.id)?.state != TaskState::WaitingToRetry
            && tracked.hold.as_ref().is_some_and(|hold| {
                hold.reason == HoldReason::Delivery && hold.task_id.as_ref() == Some(&task.id)
            })
        {
            tracked.hold = None;
            self.db.save_pull_request_source(&tracked)?;
        }
        Ok(())
    }

    /// The source moved while its delivery waited. A planned approval or merge is bound to the old
    /// head, so it cannot act on the new one; published or unconfirmed adaptations must be ours and
    /// settled before the new head is assessed under the source's budget.
    fn reassess_moved_source(&mut self, task: &Task, attempt: &Attempt) -> Result<bool> {
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let Some(source) = policy.source_pull_request.as_ref() else {
            return Ok(false);
        };
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if tracked.paused || tracked.last_task_id.as_deref() != Some(&task.id) {
            return Ok(false);
        }
        if task.delivery_mode != DeliveryMode::MergeRequest {
            return Ok(false);
        }
        let repository = &policy.repositories[&source.repository_id];
        let settled = (|| -> Result<Option<CommitSha>> {
            self.require_source_authority(task, &policy)?;
            let current = self
                .forge(&repository.forge)?
                .get_merge_request(&repository.remote, source.number)?;
            if current.state != RemoteState::Open || current.head_commit == source.input.commit {
                return Ok(None);
            }
            // An objection made meanwhile is a human decision; a new head must not bypass it.
            if self.source_changes_requested(&policy)? {
                return Ok(None);
            }
            Ok(Some(current.head_commit))
        })();
        let head = match settled {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(false),
            Err(error) if error.is_temporary() => {
                return self.schedule_delivery_retry(task, attempt, &error.to_string(), None);
            }
            Err(_) => return Ok(false),
        };
        let proposals = (|| -> Result<Option<bool>> {
            self.recover_source_proposal_receipts(task, attempt, &policy)?;
            if !self.db.list_merge_requests(&task.id)?.is_empty() {
                return Ok(Some(self.retire_superseded_adaptations(task)?));
            }
            self.require_resolved_source_proposals(task)?;
            Ok(None)
        })();
        match proposals {
            Ok(None) => {}
            Ok(Some(retired)) => return Ok(retired),
            Err(error) if error.is_temporary() => {
                return self.schedule_delivery_retry(task, attempt, &error.to_string(), None);
            }
            // An open proposal that is not exactly the planned one; a person has to look.
            Err(_) => return Ok(false),
        }
        self.retire_superseded_source(
            task,
            &attempt.id,
            tracked,
            &source.input.commit,
            &head,
            Some("delivery_retry"),
        )?;
        Ok(true)
    }

    /// The source was closed or merged by someone else while its delivery waited, which leaves
    /// nothing to deliver. Our own merge was recognized before this point.
    fn close_gone_source(&mut self, task: &Task, error: &OrchestratorError) -> Result<bool> {
        let Some(source) = source_of(task)? else {
            return Ok(false);
        };
        if error.is_temporary() || self.db.get_task(&task.id)?.state != TaskState::Working {
            return Ok(false);
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let repository = &policy.repositories[&source.repository_id];
        let open = self.forge(&repository.forge).and_then(|forge| {
            Ok(forge
                .get_merge_request(&repository.remote, source.number)?
                .state
                == RemoteState::Open)
        });
        // Published adaptations of a closed source stay visible to a person, as without a retry.
        if !matches!(open, Ok(false)) || !self.db.list_merge_requests(&task.id)?.is_empty() {
            return Ok(false);
        }
        self.db
            .set_task_state(&task.id, TaskState::Closed, "orchestrator")?;
        self.db.record_event(
            Some(&task.id),
            None,
            "orchestrator",
            "assessment.closed_source",
            serde_json::json!({"reason": "source PR is closed or merged"}),
        )?;
        Ok(true)
    }

    /// Classify holds recorded before they were typed, when that is certain. The rest keep waiting
    /// for a person.
    pub(crate) fn adopt_untyped_holds(&mut self) -> Result<()> {
        for mut source in self.db.list_pull_request_sources(None)? {
            if source.closed || !source.paused || source.hold.is_some() {
                continue;
            }
            let mut hold = Hold::new(HoldReason::Unclassified, source.status.clone());
            let mut operator_pause = source.status == "Automatic assessments paused by a human"
                || source.status == "Paused by a human";
            if let Some(task_id) = &source.last_task_id {
                let task = self.db.get_task(task_id)?;
                let events = self.db.list_events(task_id)?;
                let last = |kind: &str| {
                    events
                        .iter()
                        .filter(|event| event.kind == kind)
                        .map(|event| event.id)
                        .max()
                        .unwrap_or(0)
                };
                // A retry that a person asked for superseded an earlier pause.
                operator_pause |= last("discovery.paused") > last("publication.retry_requested");
                hold = self.new_hold(&task, HoldReason::Unclassified, source.status.clone())?;
                let failed_checks = events
                    .iter()
                    .rev()
                    .find(|event| event.kind == "task.needs_human")
                    .is_some_and(|event| event.payload["recheck"] == "failed_checks");
                if failed_checks && task.state == TaskState::NeedsHuman && !operator_pause {
                    hold.reason = HoldReason::FailedChecks;
                    hold.releases = vec![HoldRelease::GreenChecks, HoldRelease::NewHead];
                    hold.next_at = Some(Timestamp::now());
                }
            }
            if operator_pause {
                hold.reason = HoldReason::OperatorPause;
            }
            source.hold = Some(hold);
            self.db.save_pull_request_source(&source)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct HoldsReport {
    pub moved: Vec<(String, TaskState)>,
    pub errors: Vec<String>,
}
