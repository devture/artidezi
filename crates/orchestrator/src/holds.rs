// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Holds: why a change or task is stuck, and what moves it on again.
//!
//! [`Orchestrator::process_holds`] is the one place that acts on a hold's schedule. A delivery hold
//! repeats delivery of an already validated result, without another agent run. Every retry goes
//! through the same gates as the first delivery and first looks for what already happened.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::{DeliveryMode, DeliveryRetries};
use artidezi_contract::common::{CommitSha, Outcome, RepositoryAccess};
use artidezi_contract::time::{SignedDuration, Timestamp};
use artidezi_contract::{attempt::AttemptRecord, result::AgentResult};
use artidezi_forge::{ForgeError, MergeRequestState as RemoteState, RetryCause};
use artidezi_store::model::*;
use std::collections::{BTreeMap, BTreeSet};

use crate::reevaluation::Proposals;
use crate::{Orchestrator, OrchestratorError, Result, discovery::source_of};

pub(crate) const RETRY_SCHEDULED: &str = "delivery.retry_scheduled";
pub(crate) const RETRY_STARTED: &str = "delivery.retry_started";
pub(crate) const RESTARTED: &str = "a restart interrupted delivery";
/// Actors of `publication.retry_requested` that are not people.
const AUTOMATIC_ACTORS: [&str; 2] = ["orchestrator", "recovery"];

/// Whether a restart came in the middle of a delivery. Releases that posted comments without a plan
/// could leave one without any record.
pub(crate) fn restarted(events: &[Event]) -> bool {
    events.iter().any(|event| {
        let reason = event.payload["reason"].as_str().unwrap_or_default();
        (event.kind == RETRY_SCHEDULED && reason.starts_with(RESTARTED))
            || (event.kind == "task.needs_human"
                && reason.starts_with("Publication interrupted by restart"))
    })
}

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
        let releases = if reason == HoldReason::Question && task.kind == TaskKind::PullRequest {
            vec![HoldRelease::NewHead, HoldRelease::TargetMoved]
        } else {
            vec![]
        };
        self.hold_until(task, attempt, reason, message, releases)
    }

    /// Delivery stopped with `error`; the next head or target may make another evaluation useful.
    pub(crate) fn stop_delivery(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        error: &OrchestratorError,
        message: &str,
    ) -> Result<()> {
        let releases = if task.kind == TaskKind::PullRequest {
            crate::reevaluation::releases_after(error)
        } else {
            vec![]
        };
        self.hold_until(
            task,
            attempt,
            HoldReason::DeliveryStopped,
            message,
            releases,
        )
    }

    fn hold_until(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        reason: HoldReason,
        message: &str,
        releases: Vec<HoldRelease>,
    ) -> Result<()> {
        let mut hold = self.new_hold(task, reason, message)?;
        hold.releases = releases;
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
                // A newer evaluation owns the change; an older task must not stop it.
                (tracked.last_task_id.as_ref() == Some(&task.id)).then(|| {
                    tracked.place_hold(hold.clone());
                    tracked
                })
            }
            None => None,
        };
        let task_hold =
            (task.kind == TaskKind::Manual && state == TaskState::WaitingToRetry).then_some(hold);
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
                    && event.payload["backoff"] != true
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

    /// Delivery of a validated result failed: evaluate a moved source afresh, retry it later if only
    /// time is needed, else ask.
    pub(crate) fn delivery_failed(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        error: &OrchestratorError,
        message: &str,
    ) -> Result<()> {
        let mut message = message.to_owned();
        let moved = matches!(
            error,
            OrchestratorError::SourceHeadChanged { .. }
                | OrchestratorError::Forge(ForgeError::HeadMoved { .. })
        );
        if moved && self.reassess_moved_source(task, attempt)? {
            return Ok(());
        }
        if error.is_temporary() {
            if self.schedule_delivery_retry(task, attempt, &Interruption::from(error))? {
                return Ok(());
            }
            let used = self.delivery_retries_used(task, attempt)?;
            if used > 0 {
                message = format!("{message} (still failing after {used} automatic retries)");
            }
        }
        self.stop_delivery(task, attempt, error, &message)
    }

    /// Returns false when this delivery cannot or may no longer be repeated automatically.
    pub(crate) fn schedule_delivery_retry(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        interruption: &Interruption,
    ) -> Result<bool> {
        let task = self.db.get_task(&task.id)?;
        let attempt = self.db.get_attempt(&attempt.id)?;
        if attempt.state != AttemptState::Valid
            || task.state == TaskState::NeedsHuman
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
        // Waiting for a known backoff sends nothing, so it is not a failed try.
        // One that follows another was not worth waiting for, and counts.
        let after_backoff = self
            .db
            .list_events(&task.id)?
            .iter()
            .rev()
            .find(|event| {
                event.kind == RETRY_SCHEDULED && event.attempt_id.as_deref() == Some(&attempt.id)
            })
            .is_some_and(|event| event.payload["waited"] == true);
        let free = interruption.backoff && !after_backoff;
        let counted = u32::from(!free);
        if used + counted > limits.max || limits.max == 0 {
            return Ok(false);
        }
        let cause = format!("{}{}", interruption.cause, self.delivery_stage(&task)?);
        let now = Timestamp::now();
        let delay = if interruption.backoff {
            limits.initial_delay_seconds
        } else {
            limits.delay_seconds(used)
        };
        let at = now + SignedDuration::from_secs(i64::from(delay));
        let mut hold = self.new_hold(&task, HoldReason::Delivery, cause.as_str())?;
        hold.releases = vec![HoldRelease::Time];
        hold.retries = used + counted;
        hold.max_retries = limits.max;
        hold.next_at = Some(interruption.not_before.map_or(at, |wait| wait.max(at)));
        self.record_hold(
            &task,
            &attempt,
            TaskState::WaitingToRetry,
            RETRY_SCHEDULED,
            serde_json::json!({
                "reason": cause,
                "backoff": free,
                "waited": interruption.backoff,
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
        let mut holds: BTreeMap<String, Hold> = self.db.list_task_holds()?.into_iter().collect();
        // Held assessments that their source or schedule gives a reason to look at.
        let mut reconsider = BTreeSet::new();
        let mut open = BTreeSet::new();
        for source in self.db.list_pull_request_sources(None)? {
            if let Some(hold) = &source.hold
                && let Some(task_id) = &source.last_task_id
                && hold.task_id.as_ref() == Some(task_id)
            {
                if !source.closed {
                    open.insert(task_id.clone());
                }
                if Self::worth_reconsidering(&source, now) {
                    reconsider.insert(task_id.clone());
                }
                holds.insert(task_id.clone(), hold.clone());
            }
        }
        // Waiting tasks come from their own state, so that none waits without a hold to end it.
        let mut due: Vec<(Task, Option<Hold>)> = Vec::new();
        for task in self.db.list_tasks(Some(TaskState::WaitingToRetry))? {
            let hold = holds.remove(&task.id).filter(Hold::waits);
            if hold.as_ref().is_none_or(|hold| {
                hold.next_at.is_none() || hold.due(now) || reconsider.contains(&task.id)
            }) {
                due.push((task, hold));
            }
        }
        for (task_id, hold) in holds {
            if (hold.reason == HoldReason::FailedChecks && hold.due(now))
                || reconsider.contains(&task_id)
            {
                let task = self.db.get_task(&task_id)?;
                // A task can have closed with its change, which was then reopened.
                if task.state == TaskState::NeedsHuman
                    || (task.state == TaskState::Closed && open.contains(&task_id))
                {
                    due.push((task, Some(hold)));
                }
            }
        }
        let mut report = HoldsReport::default();
        for (task, hold) in due {
            let task_id = task.id.clone();
            let result = match hold {
                Some(hold) if hold.reason == HoldReason::FailedChecks => {
                    self.recheck_failed_source_checks(&task, now)
                }
                Some(hold) if hold.reason == HoldReason::Delivery => self.retry_delivery(&task),
                Some(_) => self.reconsider(&task, now),
                None => self.stop_orphaned_retry(&task),
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
        if !attempt.state.is_terminal() {
            // A crash while queuing a revision left its state behind; the revision replaces the retry.
            return self.clear_delivery_hold(task);
        }
        let used = self.delivery_retries_used(task, &attempt)?;
        let max = self.delivery_retries(task).max;
        if max == 0 || used > max {
            let releases = match task.kind {
                TaskKind::PullRequest => vec![HoldRelease::NewHead, HoldRelease::TargetMoved],
                _ => vec![],
            };
            return self.hold_until(
                task,
                &attempt,
                HoldReason::DeliveryStopped,
                "delivery retries were turned off or limited while one was scheduled",
                releases,
            );
        }
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            RETRY_STARTED,
            serde_json::json!({"retry": used}),
        )?;
        let task = self.db.get_task(&task.id)?;
        if let Err(error) = self.redeliver(&task, &attempt)
            && !self.close_gone_source(&task, &error)?
        {
            self.delivery_failed(
                &task,
                &attempt,
                &error,
                &format!("automatic delivery retry stopped: {error}"),
            )?;
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
        if task.kind == TaskKind::Manual {
            self.require_manual_authority(task)?;
        }
        // Approvals and merges check this themselves, after looking for what already happened.
        if result.outcome == Outcome::Proposal
            && task.delivery_mode == DeliveryMode::MergeRequest
            && source_of(task)?.is_some()
        {
            let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
                .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
            if self.source_changes_requested(&policy)? {
                return Err(OrchestratorError::Outdated(
                    "A reviewer has requested changes; maintainer input required".into(),
                ));
            }
        }
        self.apply_outcome(task, attempt, &job, &record, &result)
    }

    /// Source delivery compares its frozen policy with the current configuration at every gate. A
    /// manual task's first delivery follows its frozen policy alone, so a later automatic one checks
    /// that the project still permits it.
    fn require_manual_authority(&self, task: &Task) -> Result<()> {
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let permitted = self
            .config
            .projects
            .get(&task.project)
            .is_some_and(|project| {
                project.effects.publish_adaptation_merge_requests
                    && (project.effects.allow_manual_github_publication
                        || !policy.effects.allow_manual_github_publication)
                    && project.allowed_delivery_modes.contains(&task.delivery_mode)
                    && policy
                        .repositories
                        .iter()
                        .filter(|(_, repo)| repo.access == RepositoryAccess::ReadWrite)
                        .all(|(id, repo)| {
                            project.repositories.get(id).is_some_and(|current| {
                                current.forge == repo.forge
                                    && current.remote == repo.remote
                                    && current.branch == repo.branch
                                    && current.agent_access == RepositoryAccess::ReadWrite
                                    && current.allowed_paths == repo.allowed_paths
                            }) && self.config.forges.get(&repo.forge).is_some_and(|forge| {
                                forge.provider == repo.forge_provider
                                    && repo
                                        .forge_api_base_url
                                        .as_ref()
                                        .is_none_or(|url| *url == forge.api_base_url)
                                    && repo
                                        .forge_bot_login
                                        .as_ref()
                                        .is_none_or(|login| *login == forge.bot_login)
                            })
                        })
            });
        if permitted {
            Ok(())
        } else {
            Err(OrchestratorError::Invalid(
                "current configuration no longer permits publication".into(),
            ))
        }
    }

    /// A pause interrupted by a crash, or a hold placed for another reason, leaves a waiting task
    /// that nothing would pick up.
    fn stop_orphaned_retry(&mut self, task: &Task) -> Result<()> {
        if let Some(source) = source_of(task)?
            && self
                .db
                .get_pull_request_source(&source.tracking_id)?
                .last_task_id
                .as_ref()
                != Some(&task.id)
        {
            // A newer evaluation replaced it, but a crash kept it from being closed.
            return Ok(self
                .db
                .set_task_state(&task.id, TaskState::Closed, "orchestrator")?);
        }
        match self.db.latest_attempt(&task.id)? {
            Some(attempt) => self.needs_human(
                task,
                &attempt,
                "delivery stopped while it waited for a retry",
            ),
            None => Ok(self
                .db
                .set_task_state(&task.id, TaskState::NeedsHuman, "orchestrator")?),
        }
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

    /// The source moved before delivery finished. A planned approval or merge is bound to the old
    /// head, so it cannot act on the new one; published or unconfirmed adaptations must be ours and
    /// settled before the new head is assessed under the source's budget.
    pub(crate) fn reassess_moved_source(&mut self, task: &Task, attempt: &Attempt) -> Result<bool> {
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let Some(source) = policy.source_pull_request.as_ref() else {
            return Ok(false);
        };
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if tracked.paused || tracked.last_task_id.as_deref() != Some(&task.id) {
            return Ok(false);
        }
        // Without budget the hold remains, so that a mere rebase can still use its own retry.
        let within_budget = self
            .config
            .projects
            .get(&tracked.project)
            .and_then(|project| project.pull_requests.as_ref())
            .is_some_and(|discovery| tracked.runs < discovery.max_runs_per_pull_request);
        if task.delivery_mode != DeliveryMode::MergeRequest || !within_budget {
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
            // An objection or an edit made meanwhile is a human decision; a new head must not bypass
            // it.
            if self.person_objects(task, source, &tracked)? {
                return Ok(None);
            }
            Ok(Some(current.head_commit))
        })();
        let head = match settled {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(false),
            Err(error) if error.is_temporary() => {
                return self.schedule_delivery_retry(task, attempt, &Interruption::from(&error));
            }
            Err(_) => return Ok(false),
        };
        match self.settle_proposals(task, attempt, &policy) {
            Ok(Proposals::Settled) => {}
            Ok(Proposals::Retired) => return Ok(true),
            Ok(Proposals::Unsettled) => return Ok(false),
            Err(error) if error.is_temporary() => {
                return self.schedule_delivery_retry(task, attempt, &Interruption::from(&error));
            }
            Err(error) => return Err(error),
        }
        self.retire_superseded_source(
            task,
            &attempt.id,
            tracked,
            &source.input.commit,
            &head,
            Some("moved_before_delivery"),
            None,
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
        for source in self.db.list_pull_request_sources(None)? {
            self.adopt_untyped_hold(source)?;
        }
        Ok(())
    }

    /// Discovery adopts those of changes that were closed at startup once they are open again.
    pub(crate) fn adopt_untyped_hold(&mut self, mut source: TrackedPullRequest) -> Result<()> {
        if !source.paused || source.hold.is_some() {
            return Ok(());
        }
        let hold = self.classify_untyped_hold(&source).unwrap_or_else(|error| {
            tracing::warn!(source = %source.id, %error, "could not classify an older hold");
            Hold::new(HoldReason::Unclassified, source.status.clone())
        });
        // Of a closed change, only a person's pause outlives a reopening.
        if source.closed && hold.reason != HoldReason::OperatorPause {
            return Ok(());
        }
        // In one step, so that a crash leaves either the untyped hold or its classification.
        if hold.reason == HoldReason::Unclassified
            && let Some(task_id) = &hold.task_id
        {
            let task = self.db.get_task(task_id)?;
            if matches!(task.state, TaskState::NeedsHuman | TaskState::Closed) {
                match self.adopt_held_task(&task, &source.status) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        tracing::warn!(source = %source.id, %error, "could not adopt an older hold");
                    }
                }
            }
        }
        source.hold = Some(hold);
        self.db.save_pull_request_source(&source)?;
        Ok(())
    }

    fn classify_untyped_hold(&self, source: &TrackedPullRequest) -> Result<Hold> {
        let mut hold = Hold::new(HoldReason::Unclassified, source.status.clone());
        if source.status == "Automatic assessments paused by a human"
            || source.status == "Paused by a human"
        {
            hold.reason = HoldReason::OperatorPause;
        }
        let Some(task_id) = &source.last_task_id else {
            return Ok(hold);
        };
        hold.task_id = Some(task_id.clone());
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
        if last("discovery.paused") > last("publication.retry_requested") {
            hold.reason = HoldReason::OperatorPause;
        }
        if hold.reason == HoldReason::OperatorPause {
            return Ok(hold);
        }
        let task = self.db.get_task(task_id)?;
        // Closing a task by hand pauses its change; discovery closes it along with the change.
        let closed_by = events
            .iter()
            .rev()
            .find(|event| event.kind == "task.state_changed" && event.payload["to"] == "closed")
            .map(|event| event.actor.as_str());
        if task.state == TaskState::Closed && closed_by != Some("discovery") {
            hold.reason = HoldReason::OperatorPause;
            return Ok(hold);
        }
        hold = self.new_hold(&task, HoldReason::Unclassified, source.status.clone())?;
        let failed_checks = events
            .iter()
            .rev()
            .find(|event| event.kind == "task.needs_human")
            .is_some_and(|event| event.payload["recheck"] == "failed_checks");
        if failed_checks && matches!(task.state, TaskState::NeedsHuman | TaskState::Closed) {
            hold.reason = HoldReason::FailedChecks;
            hold.releases = vec![HoldRelease::GreenChecks, HoldRelease::NewHead];
            hold.next_at = Some(Timestamp::now());
        }
        Ok(hold)
    }
}

/// What interrupted a delivery.
pub(crate) struct Interruption {
    cause: String,
    not_before: Option<Timestamp>,
    /// Nothing was tried: an earlier answer asked to wait.
    backoff: bool,
}

impl Interruption {
    pub(crate) fn new(cause: &str) -> Self {
        Self {
            cause: cause.into(),
            not_before: None,
            backoff: false,
        }
    }
}

impl From<&OrchestratorError> for Interruption {
    fn from(error: &OrchestratorError) -> Self {
        let (not_before, backoff) = match error {
            OrchestratorError::Forge(ForgeError::RetryAfter { retry_at, cause }) => {
                (Some(*retry_at), *cause == RetryCause::Backoff)
            }
            _ => (None, false),
        };
        Self {
            cause: error.to_string(),
            not_before,
            backoff,
        }
    }
}

#[derive(Default)]
pub(crate) struct HoldsReport {
    pub moved: Vec<(String, TaskState)>,
    pub errors: Vec<String>,
}
