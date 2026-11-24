// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Evaluating a held change again: after a failed run, a temporary blocker, or once the change or its target moved.
//!
//! Discovery only notes what it sees. [`Orchestrator::reconsider`] decides, under the gates that a
//! person's request would meet, and hands the change back to discovery for an ordinary evaluation
//! of its current head, target, checks and configuration. A change that differs uses the run
//! budget; the same change uses the separately limited retries.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::PullRequestDiscovery;
use artidezi_config::resolve::PullRequestSource;
use artidezi_contract::common::{CommitSha, Outcome};
use artidezi_contract::result::AgentResult;
use artidezi_contract::time::{SignedDuration, Timestamp};
use artidezi_forge::{
    ForgeError, MergeRequestInfo, MergeRequestReview, MergeRequestState as RemoteState, ReviewState,
};
use artidezi_store::model::*;

use crate::discovery::{FOLLOW_UP_INTERVAL_SECONDS, source_of};
use crate::{Orchestrator, OrchestratorError, Result};

pub(crate) const EVALUATION_RETRY_SCHEDULED: &str = "evaluation.retry_scheduled";
/// How long something that only a person can settle keeps a moved change from another look.
const BLOCKED_SECONDS: i64 = 600;

/// What became of the proposals of an assessment that is about to be replaced.
pub(crate) enum Proposals {
    /// None is open, so that a new evaluation may start.
    Settled,
    /// Obsolete ones were closed and their task retired.
    Retired,
    /// A person has to settle them.
    Unsettled,
}

/// Whether a failed run may be followed by another one.
enum RunRetry {
    Allowed,
    Exhausted,
    /// The run broke a rule, or another one would fail for the same reason.
    Never,
}

impl Orchestrator {
    fn discovery_of(&self, project: &str) -> Option<&PullRequestDiscovery> {
        self.config.projects.get(project)?.pull_requests.as_ref()
    }

    /// How many evaluations of this kind a change gets, and the wait before the next one after
    /// `used` of them, in seconds.
    fn evaluation_retries(&self, project: &str, kind: EvaluationRetry, used: u32) -> (u32, u32) {
        let Some(discovery) = self.discovery_of(project) else {
            return (0, 0);
        };
        let retries = &discovery.retries;
        match kind {
            EvaluationRetry::Failed => (retries.failed.max, retries.failed.initial_delay_seconds),
            EvaluationRetry::TargetChange => (
                retries.target_change.max,
                retries.target_change.initial_delay_seconds,
            ),
            EvaluationRetry::RetryLater => (
                retries.retry_later.max,
                retries.retry_later.delay_seconds(used),
            ),
            EvaluationRetry::Unknown => (0, 0),
        }
    }

    /// Whether evaluations of this project's changes may end in `retry_later`.
    pub(crate) fn offers_retry_later(&self, project: &str) -> bool {
        self.evaluation_retries(project, EvaluationRetry::RetryLater, 0)
            .0
            > 0
    }

    pub(crate) fn evaluation_retry_left(
        &self,
        source: &TrackedPullRequest,
        kind: EvaluationRetry,
    ) -> bool {
        let used = source.reevaluations.used(kind);
        used < self.evaluation_retries(&source.project, kind, used).0
    }

    /// Announces the next evaluation of this kind on `hold`.
    fn schedule_evaluation(
        &self,
        source: &TrackedPullRequest,
        hold: &mut Hold,
        kind: EvaluationRetry,
        now: Timestamp,
    ) {
        let used = source.reevaluations.used(kind);
        let (max, delay) = self.evaluation_retries(&source.project, kind, used);
        hold.retry = Some(kind);
        hold.retries = used + 1;
        hold.max_retries = max;
        hold.next_at = Some(now + SignedDuration::from_secs(i64::from(delay)));
    }

    /// A run of a source evaluation broke, or produced no acceptable result: try once more later,
    /// else ask.
    pub(crate) fn run_failed(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        message: &str,
    ) -> Result<()> {
        self.retry_run(task, attempt, EvaluationRetry::Failed, message.to_owned())
    }

    /// The agent reported a temporary blocker: evaluate again later, else ask.
    pub(crate) fn run_postponed(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        result: &AgentResult,
    ) -> Result<()> {
        let reason = excerpt(result.retry_reason.as_deref().unwrap_or_default(), 500);
        self.retry_run(task, attempt, EvaluationRetry::RetryLater, reason)
    }

    /// A restart came between accepting a result that needs no delivery and acting on it.
    pub(crate) fn resume_undelivered_run(
        &mut self,
        task: &Task,
        attempt: &Attempt,
    ) -> Result<bool> {
        if task.kind != TaskKind::PullRequest {
            return Ok(false);
        }
        match attempt.outcome {
            Some(Outcome::Failed) => {
                self.run_failed(task, attempt, "The agent reported failure")?;
            }
            Some(Outcome::RetryLater) => {
                let Some(result) = &attempt.result else {
                    return Ok(false);
                };
                match self.artifacts.get_json::<AgentResult>(result) {
                    Ok(result) => self.run_postponed(task, attempt, &result)?,
                    // Startup goes on for the other tasks.
                    Err(error) => self.run_failed(
                        task,
                        attempt,
                        &format!("The result of the interrupted run cannot be read: {error}"),
                    )?,
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn retry_run(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        kind: EvaluationRetry,
        message: String,
    ) -> Result<()> {
        let task = self.db.get_task(&task.id)?;
        let attempt = self.db.get_attempt(&attempt.id)?;
        let reason = match kind {
            EvaluationRetry::RetryLater => HoldReason::TemporaryBlocker,
            _ => HoldReason::FailedRun,
        };
        let mut hold = self.new_hold(&task, reason, message)?;
        match self.run_retry(&task, &attempt, kind)? {
            RunRetry::Never => {}
            RunRetry::Exhausted => {
                hold.message = self.exhausted_message(&task, kind, hold.message)?;
                hold.releases = vec![HoldRelease::NewHead, HoldRelease::TargetMoved];
            }
            RunRetry::Allowed => {
                let source = source_of(&task)?.expect("checked source");
                let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
                hold.releases = vec![
                    HoldRelease::Time,
                    HoldRelease::NewHead,
                    HoldRelease::TargetMoved,
                ];
                let now = Timestamp::now();
                self.schedule_evaluation(&tracked, &mut hold, kind, now);
                return self.record_hold(
                    &task,
                    &attempt,
                    TaskState::WaitingToRetry,
                    EVALUATION_RETRY_SCHEDULED,
                    serde_json::json!({
                        "reason": hold.message,
                        "kind": kind,
                        "retry": hold.retries,
                        "max_retries": hold.max_retries,
                        "at": hold.next_at,
                    }),
                    &hold,
                );
            }
        }
        let payload = serde_json::json!({"reason": hold.message, "hold": hold.reason});
        self.record_hold(
            &task,
            &attempt,
            TaskState::NeedsHuman,
            "task.needs_human",
            payload,
            &hold,
        )
    }

    /// A blocker that outlasted its retries says so; a failed run's message needs nothing.
    fn exhausted_message(
        &self,
        task: &Task,
        kind: EvaluationRetry,
        message: String,
    ) -> Result<String> {
        let Some(source) = source_of(task)? else {
            return Ok(message);
        };
        let used = self
            .db
            .get_pull_request_source(&source.tracking_id)?
            .reevaluations
            .used(kind);
        Ok(match kind {
            EvaluationRetry::RetryLater if used == 1 => {
                format!("Still blocked after 1 retry: {message}")
            }
            EvaluationRetry::RetryLater if used > 1 => {
                format!("Still blocked after {used} retries: {message}")
            }
            _ => message,
        })
    }

    /// The temporary blocker that the source's last evaluation reported, for the next one to check.
    pub(crate) fn last_blocker(&self, source: &TrackedPullRequest) -> Result<Option<String>> {
        let Some(task_id) = &source.last_task_id else {
            return Ok(None);
        };
        let Some(attempt) = self.db.latest_attempt(task_id)? else {
            return Ok(None);
        };
        if attempt.outcome != Some(Outcome::RetryLater) || attempt.state != AttemptState::Valid {
            return Ok(None);
        }
        // Only context, so that a result that is gone must not prevent the evaluation.
        let result: Option<AgentResult> = attempt
            .result
            .and_then(|result| self.artifacts.get_json(&result).ok());
        Ok(result
            .and_then(|result| result.retry_reason)
            .map(|reason| {
                format!(
                    "An earlier evaluation of this change, at {}, was postponed because of a temporary blocker. Check whether it still applies. It reported:\n{}",
                    attempt.updated_at.to_rfc3339(),
                    excerpt(&reason, 2000)
                )
            }))
    }

    fn run_retry(&self, task: &Task, attempt: &Attempt, kind: EvaluationRetry) -> Result<RunRetry> {
        let Some(source) = source_of(task)? else {
            return Ok(RunRetry::Never);
        };
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let broke = match attempt.state {
            AttemptState::Lost => true,
            AttemptState::Invalid => {
                !attempt
                    .validation_problems
                    .iter()
                    .any(|problem| crate::ingest::broke_a_rule(problem))
                    && !self.model_refused(attempt)
            }
            AttemptState::Valid => {
                attempt.outcome
                    == Some(match kind {
                        EvaluationRetry::RetryLater => Outcome::RetryLater,
                        _ => Outcome::Failed,
                    })
            }
            _ => false,
        };
        // A proposal published earlier in this task is still there.
        if !broke || !self.db.list_merge_requests(&task.id)?.is_empty() {
            return Ok(RunRetry::Never);
        }
        Ok(
            if task.state.can_transition_to(TaskState::WaitingToRetry)
                && !tracked.paused
                && !tracked.closed
                && tracked.last_task_id.as_deref() == Some(&task.id)
                && self.evaluation_retry_left(&tracked, kind)
            {
                RunRetry::Allowed
            } else {
                RunRetry::Exhausted
            },
        )
    }

    /// Whether the model provider refused the run over credentials or quota, or the run reached its
    /// own budget, which no later run fixes.
    fn model_refused(&self, attempt: &Attempt) -> bool {
        (|| -> Option<bool> {
            let record: artidezi_contract::attempt::AttemptRecord = self
                .artifacts
                .get_json(attempt.attempt_record.as_ref()?)
                .ok()?;
            let stdout = record.harness.stdout.as_ref()?;
            let tail = self.artifacts.tail(stdout, 64 * 1024).ok()?;
            Some(artidezi_contract::transient::refused(
                &record.harness.name,
                &tail,
            ))
        })()
        .unwrap_or(false)
    }

    /// Discovery saw a held change again: note that its target moved, which earns the same change
    /// one more evaluation after a while. A moved head needs no note, as tracking already has it.
    pub(crate) fn note_target_change(
        &self,
        source: &mut TrackedPullRequest,
        head: &CommitSha,
        now: Timestamp,
    ) {
        let Some(hold) = &source.hold else {
            return;
        };
        let repository = self
            .config
            .projects
            .get(&source.project)
            .and_then(|project| project.repositories.get(&source.repository_id))
            .filter(|repo| repo.forge == source.forge && repo.remote == source.remote);
        let (Some(repository), Some(target)) = (repository, &hold.target) else {
            return;
        };
        if hold.retry.is_some()
            || !reconsiderable(hold)
            || !hold.releases_on(HoldRelease::TargetMoved)
            || hold.head.as_deref() != Some(head.as_str())
            || hold.task_id != source.last_task_id
            || hold.blocked_until.is_some_and(|until| until > now)
            || !self.evaluation_retry_left(source, EvaluationRetry::TargetChange)
        {
            return;
        }
        let current = self
            .forge(&repository.forge)
            .and_then(|forge| Ok(forge.branch_head(&repository.remote, &repository.branch)?));
        match current {
            Ok(current) if current.as_str() != target => {
                let mut hold = hold.clone();
                self.schedule_evaluation(source, &mut hold, EvaluationRetry::TargetChange, now);
                source.hold = Some(hold);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(source = %source.id, %error, "target of a held change not read");
            }
        }
    }

    /// Whether a held change has anything to decide now. Reads stored state only.
    pub(crate) fn worth_reconsidering(source: &TrackedPullRequest, now: Timestamp) -> bool {
        source.hold.as_ref().is_some_and(|hold| {
            let moved = hold.releases_on(HoldRelease::NewHead)
                && hold
                    .head
                    .as_ref()
                    .is_some_and(|head| *head != source.observed_head)
                && hold.rebased_head.as_ref() != Some(&source.observed_head)
                && hold.blocked_until.is_none_or(|until| until <= now);
            reconsiderable(hold)
                && !source.closed
                && (moved || (hold.retry.is_some() && hold.due(now)))
        })
    }

    /// Decide whether a held change is evaluated again now, later or not yet.
    pub(crate) fn reconsider(&mut self, task: &Task, now: Timestamp) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let Some(hold) = tracked.hold.clone() else {
            return Ok(());
        };
        let due = hold.due(now);
        // Reserve the next look before contacting the forge, so restarts cannot hot-loop.
        let again = now + SignedDuration::from_secs(FOLLOW_UP_INTERVAL_SECONDS);
        self.update_hold(&source.tracking_id, |hold| {
            hold.blocked_until = Some(again);
            if due {
                hold.next_at = Some(again);
            }
        })?;
        match self.reconsider_inner(task, &source, &hold, due, now) {
            Ok(Verdict::Done) => Ok(()),
            Ok(Verdict::Blocked(reason)) => {
                tracing::debug!(task = %task.id, reason, "held change not evaluated again yet");
                self.stop_waiting(task, due, now)
            }
            Err(error) if error.is_temporary() => Ok(()),
            Err(error) => {
                tracing::warn!(task = %task.id, %error, "held change could not be reconsidered");
                self.stop_waiting(task, due, now)
            }
        }
    }

    fn reconsider_inner(
        &mut self,
        task: &Task,
        source: &PullRequestSource,
        hold: &Hold,
        due: bool,
        now: Timestamp,
    ) -> Result<Verdict> {
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(Verdict::Blocked("no attempt"));
        };
        let watched = self
            .discovery_of(&tracked.project)
            .is_some_and(|discovery| {
                discovery.repositories.contains(&tracked.repository_id)
                    && self.config.projects[&tracked.project]
                        .repositories
                        .get(&tracked.repository_id)
                        .is_some_and(|repo| {
                            repo.forge == tracked.forge && repo.remote == tracked.remote
                        })
            });
        if !watched || !attempt.state.is_terminal() {
            return Ok(Verdict::Blocked("not watched, or still running"));
        }
        let current = self
            .forge(&tracked.forge)?
            .get_merge_request(&tracked.remote, source.number)?;
        if current.state != RemoteState::Open {
            // Discovery reconciles closed and merged sources.
            return Ok(Verdict::Blocked("source is closed"));
        }
        if self.person_objects(task, source, &tracked)? {
            return Ok(Verdict::Blocked("a person objects"));
        }
        let moved = hold.head.as_deref() != Some(current.head_commit.as_str());
        if moved {
            if !hold.releases_on(HoldRelease::NewHead) {
                return Ok(Verdict::Blocked("not released by a new head"));
            }
            if hold.rebased_head.as_deref() != Some(current.head_commit.as_str())
                && !self.held_change_is_unchanged(source, &tracked, hold, &current)?
            {
                return self.evaluate_again(task, &attempt, &policy, &current.head_commit, None);
            }
        }
        if due && let Some(kind) = hold.retry {
            let applies = match kind {
                EvaluationRetry::Failed | EvaluationRetry::RetryLater => true,
                EvaluationRetry::TargetChange => moved || self.target_moved(&tracked, hold)?,
                EvaluationRetry::Unknown => false,
            };
            if !applies || !self.evaluation_retry_left(&tracked, kind) {
                return Ok(Verdict::Blocked("nothing changed, or no retry is left"));
            }
            return self.evaluate_again(task, &attempt, &policy, &current.head_commit, Some(kind));
        }
        if moved {
            let schedule = hold.retry.is_none()
                && self.evaluation_retry_left(&tracked, EvaluationRetry::TargetChange);
            let mut next = hold.clone();
            if schedule {
                self.schedule_evaluation(&tracked, &mut next, EvaluationRetry::TargetChange, now);
            }
            next.rebased_head = Some(current.head_commit.0);
            next.blocked_until = None;
            self.update_hold(&tracked.id, |hold| *hold = next)?;
        }
        Ok(Verdict::Done)
    }

    fn target_moved(&self, tracked: &TrackedPullRequest, hold: &Hold) -> Result<bool> {
        let repository =
            &self.config.projects[&tracked.project].repositories[&tracked.repository_id];
        let target = self
            .forge(&repository.forge)?
            .branch_head(&repository.remote, &repository.branch)?;
        Ok(hold.target.as_deref() != Some(target.as_str()))
    }

    /// Whether a reviewer's latest decision requests changes, or our assessment of this task was
    /// edited, deleted or dismissed. Either is a human decision that a new head or target must not bypass.
    pub(crate) fn person_objects(
        &self,
        task: &Task,
        source: &PullRequestSource,
        tracked: &TrackedPullRequest,
    ) -> Result<bool> {
        if self.changes_requested(&tracked.forge, &tracked.remote, source.number)? {
            return Ok(true);
        }
        let events = self.db.list_events(&task.id)?;
        let published: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "assessment.published")
            .filter_map(|event| Some((event.payload["review_id"].as_u64()?, event)))
            .collect();
        let planned: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "assessment.approval_planned" | "assessment.comment_planned"
                )
            })
            .map(|event| &event.payload)
            .collect();
        if published.is_empty() && planned.is_empty() {
            return Ok(false);
        }
        let reviews = self
            .forge(&tracked.forge)?
            .list_reviews(&tracked.remote, source.number)?;
        let marker = format!("<!-- artidezi:assessment:{} -->", task.id);
        // Plans and receipts of older releases can lack what was posted. It is built from the result.
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let as_built = |attempt: Option<&str>, review: &MergeRequestReview| -> Result<bool> {
            let attempt = match attempt {
                Some(id) => Some(self.db.get_attempt(id)?),
                None => self.db.latest_attempt(&task.id)?,
            };
            let Some(result) = attempt
                .and_then(|attempt| attempt.result)
                .and_then(|result| self.artifacts.get_json::<AgentResult>(&result).ok())
                .filter(|result| !matches!(result.outcome, Outcome::RetryLater | Outcome::Failed))
            else {
                return Ok(false);
            };
            for (automatic, without_checks) in [(false, false), (true, false), (true, true)] {
                if review.body
                    == self.assessment_body(task, &result, &policy, automatic, without_checks)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        // Forges dismiss approvals themselves when the head moves.
        let moved = tracked.observed_head != source.input.commit.as_str();
        if published.is_empty() {
            // Its receipt was lost: it can have arrived, and have been edited since.
            for review in &reviews {
                let ours = review.body.contains(&marker);
                let altered = Some(&review.author_login) == source.publisher_login.as_ref()
                    && review.head_commit == source.input.commit
                    && !review.body.contains("<!-- artidezi:assessment:");
                if altered || (ours && review.state == ReviewState::Dismissed && !moved) {
                    return Ok(true);
                }
                if !ours {
                    continue;
                }
                let mut as_planned = false;
                for plan in &planned {
                    as_planned |= if let Some(body) = plan["body"].as_str() {
                        review.body == body
                    } else if let Some(sha256) = plan["body_sha256"].as_str() {
                        crate::source_publish::digest(&review.body) == sha256
                    } else {
                        as_built(None, review)?
                    };
                }
                if !as_planned {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        for (id, event) in published {
            let receipt = &event.payload;
            let Some(review) = reviews.iter().find(|review| review.id == id) else {
                return Ok(true);
            };
            let mut bodies = planned
                .iter()
                .filter(|_| receipt["state"] == "approved")
                .filter_map(|plan| plan["body"].as_str())
                .peekable();
            let unedited = if let Some(digest) = receipt["body_sha256"].as_str() {
                digest == crate::source_publish::digest(&review.body)
            } else if bodies.peek().is_some() {
                bodies.any(|body| body == review.body)
            } else {
                as_built(event.attempt_id.as_deref(), review)?
            };
            if !unedited
                || !review.body.contains(&marker)
                || (review.state == ReviewState::Dismissed
                    && !(receipt["state"] == "approved" && moved))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fetches the current head and target and compares the change with the held assessment's.
    /// Ambiguous or missing history counts as a different change, which the run budget bounds.
    fn held_change_is_unchanged(
        &self,
        source: &PullRequestSource,
        tracked: &TrackedPullRequest,
        hold: &Hold,
        current: &MergeRequestInfo,
    ) -> Result<bool> {
        let (Some(target), Some(head)) = (&hold.target, &hold.head) else {
            return Ok(false);
        };
        let repository =
            &self.config.projects[&tracked.project].repositories[&tracked.repository_id];
        self.same_source_change(
            (&repository.forge, &repository.remote, &repository.branch),
            source,
            (target, head),
            &current.head_commit,
        )
        .map(|same| same.is_some())
    }

    /// The fresh target when `head` carries the same change against it as `before` did.
    pub(crate) fn same_source_change(
        &self,
        (forge_name, remote, branch): (&str, &str, &str),
        source: &PullRequestSource,
        before: (&str, &str),
        head: &CommitSha,
    ) -> Result<Option<CommitSha>> {
        let forge = self.forge(forge_name)?;
        let target = forge.branch_head(remote, branch)?;
        let remote_ref = forge.merge_request_ref(remote, source.number)?;
        let cache = self.cache(forge_name, remote)?;
        let target_ref = format!("refs/heads/{branch}");
        let source_ref = format!("refs/artidezi/discovery/{}", source.tracking_id);
        cache.fetch(
            &forge.clone_url(remote),
            &[
                &format!("+{target_ref}:{target_ref}"),
                &format!("+{remote_ref}:{source_ref}"),
            ],
            forge.git_credentials(remote)?.as_ref(),
        )?;
        let target_oid = git2::Oid::from_str(target.as_str())?;
        let head_oid = git2::Oid::from_str(head.as_str())?;
        if cache.resolve(&target_ref)? != target_oid || cache.resolve(&source_ref)? != head_oid {
            return Err(ForgeError::Temporary(
                "the change or its target moved while it was read".into(),
            )
            .into());
        }
        let same = artidezi_git::equivalence::same_change(
            cache.repo(),
            (
                git2::Oid::from_str(before.0)?,
                git2::Oid::from_str(before.1)?,
            ),
            (target_oid, head_oid),
        )
        .unwrap_or(false);
        Ok(same.then_some(target))
    }

    /// Retire the held assessment and let discovery evaluate the change again. `kind` is `None` for
    /// a change that differs, which uses the run budget.
    fn evaluate_again(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
        head: &CommitSha,
        kind: Option<EvaluationRetry>,
    ) -> Result<Verdict> {
        let source = policy
            .source_pull_request
            .as_ref()
            .expect("checked source identity");
        match self.settle_proposals(task, attempt, policy)? {
            Proposals::Settled => {}
            Proposals::Retired => return Ok(Verdict::Done),
            Proposals::Unsettled => return Ok(Verdict::Blocked("proposals are unresolved")),
        }
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if kind.is_none()
            && self
                .discovery_of(&tracked.project)
                .is_none_or(|discovery| tracked.runs >= discovery.max_runs_per_pull_request)
        {
            // The question stays visible as such; only its automatic release ends here.
            return Ok(Verdict::Blocked("evaluation budget reached"));
        }
        let cause = match kind {
            Some(EvaluationRetry::Failed) => "failed_run",
            Some(EvaluationRetry::RetryLater) => "temporary_blocker",
            Some(_) => "target_change",
            None => "held_change",
        };
        self.retire_superseded_source(
            task,
            &attempt.id,
            tracked,
            &source.input.commit,
            head,
            Some(cause),
            kind,
        )?;
        Ok(Verdict::Done)
    }

    /// Account for every proposal the task planned, closing its own obsolete ones.
    pub(crate) fn settle_proposals(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
    ) -> Result<Proposals> {
        if self.db.list_merge_requests(&task.id)?.is_empty()
            && !self
                .db
                .list_events(&task.id)?
                .iter()
                .any(|event| event.kind == "publication.planned")
        {
            return Ok(Proposals::Settled);
        }
        let settled = (|| -> Result<Proposals> {
            self.require_source_authority(task, policy)?;
            self.recover_source_proposal_receipts(task, attempt, policy)?;
            if !self.db.list_merge_requests(&task.id)?.is_empty() {
                return Ok(if self.retire_superseded_adaptations(task)? {
                    Proposals::Retired
                } else {
                    Proposals::Unsettled
                });
            }
            self.require_resolved_source_proposals(task)?;
            Ok(Proposals::Settled)
        })();
        match settled {
            Err(error) if !error.is_temporary() => {
                // An open proposal that is not exactly the planned one; a person has to look.
                tracing::debug!(task = %task.id, %error, "proposals are unresolved");
                Ok(Proposals::Unsettled)
            }
            other => other,
        }
    }

    /// Nothing can be evaluated now. A question keeps waiting for its answer; a failed run that
    /// waited for its retry needs a person after all.
    fn stop_waiting(&mut self, task: &Task, due: bool, now: Timestamp) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("missing source identity".into()))?;
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        let Some(mut hold) = tracked.hold.clone() else {
            return Ok(());
        };
        hold.blocked_until = Some(now + SignedDuration::from_secs(BLOCKED_SECONDS));
        if !due {
            // Only the look at a moved head was blocked; a scheduled evaluation stays.
            return self.update_hold(&tracked.id, |current| *current = hold);
        }
        hold.retry = None;
        hold.retries = 0;
        hold.max_retries = 0;
        hold.next_at = None;
        hold.releases
            .retain(|release| *release != HoldRelease::Time);
        if task.state != TaskState::WaitingToRetry {
            return self.update_hold(&tracked.id, |current| *current = hold);
        }
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(());
        };
        let payload = serde_json::json!({"reason": hold.message, "hold": hold.reason});
        self.record_hold(
            task,
            &attempt,
            TaskState::NeedsHuman,
            "task.needs_human",
            payload,
            &hold,
        )
    }

    fn update_hold(&self, source_id: &str, change: impl FnOnce(&mut Hold)) -> Result<()> {
        let mut tracked = self.db.get_pull_request_source(source_id)?;
        if let Some(hold) = &mut tracked.hold {
            change(hold);
            self.db.save_pull_request_source(&tracked)?;
        }
        Ok(())
    }

    /// Classify a task that waits for a person without a typed hold: one from an older release, or
    /// one whose run was given up on by lease recovery. What is not certain stays with a person.
    pub(crate) fn adopt_held_task(&mut self, task: &Task, status: &str) -> Result<()> {
        let Some(source) = source_of(task)? else {
            return Ok(());
        };
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return self.pause_task_source(task, HoldReason::Unclassified, status);
        };
        // Older releases stopped looking at a run's commits after any of these, so that one can have
        // broken a rule without saying so.
        let uninspected = attempt.state == AttemptState::Invalid
            && attempt.validation_problems.iter().any(|problem| {
                [
                    "reported unchanged but HEAD moved",
                    "head commit mismatch between",
                    "series invalid",
                    "series has no bundle",
                ]
                .iter()
                .any(|text| problem.contains(text))
            });
        if uninspected {
            return self.pause_task_source(task, HoldReason::Unclassified, status);
        }
        let failure = match (attempt.state, attempt.outcome) {
            (AttemptState::Lost, _) => Some("The worker did not finish"),
            (AttemptState::Invalid, _) => Some("The worker result was rejected"),
            _ => None,
        };
        let reason = self
            .db
            .list_events(&task.id)?
            .iter()
            .rev()
            .find(|event| event.kind == "task.needs_human")
            .and_then(|event| event.payload["reason"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let answered = [
            "assessment published; maintainer input required",
            "shadow assessment needs human input",
            "agent reported failure; nothing published",
        ]
        .contains(&reason.as_str());
        let failure = failure.or((answered && attempt.outcome == Some(Outcome::Failed))
            .then_some("The agent reported failure"));
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if let Some(message) = failure {
            // Not a pause of its own: the failure was all that stopped the change.
            tracked.release_hold();
            self.db.save_pull_request_source(&tracked)?;
            return self.run_failed(task, &attempt, message);
        }
        let mut hold = if answered && attempt.outcome == Some(Outcome::NeedsHuman) {
            self.new_hold(task, HoldReason::Question, reason.as_str())?
        } else if attempt.state == AttemptState::Valid {
            let mut hold = self.new_hold(task, HoldReason::Unclassified, status)?;
            hold.releases = untyped_releases(&reason);
            hold
        } else {
            self.new_hold(task, HoldReason::Unclassified, status)?
        };
        if hold.reason == HoldReason::Question {
            hold.releases = vec![HoldRelease::NewHead, HoldRelease::TargetMoved];
        }
        tracked.place_hold(hold);
        self.db.save_pull_request_source(&tracked)?;
        Ok(())
    }
}

/// Agent text on one line and of bounded length.
fn excerpt(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

enum Verdict {
    Done,
    Blocked(&'static str),
}

/// Holds of an assessment that a new evaluation can replace.
fn reconsiderable(hold: &Hold) -> bool {
    matches!(
        hold.reason,
        HoldReason::Question
            | HoldReason::FailedRun
            | HoldReason::TemporaryBlocker
            | HoldReason::DeliveryStopped
            | HoldReason::Unclassified
    )
}

/// What releases a stopped delivery. Anything that can stem from a person's decision, revoked
/// permission or foreign edit stays with a person.
pub(crate) fn releases_after(error: &OrchestratorError) -> Vec<HoldRelease> {
    match error {
        OrchestratorError::SourceHeadChanged { .. }
        | OrchestratorError::Forge(ForgeError::HeadMoved { .. } | ForgeError::NotMergeable(..))
        | OrchestratorError::Git(artidezi_git::GitError::Conflict(_)) => {
            vec![HoldRelease::NewHead, HoldRelease::TargetMoved]
        }
        error if error.is_temporary() => vec![HoldRelease::NewHead, HoldRelease::TargetMoved],
        OrchestratorError::Outdated(_) => vec![HoldRelease::NewHead],
        _ => vec![],
    }
}

/// The same for a reason that an older release stored as text.
fn untyped_releases(reason: &str) -> Vec<HoldRelease> {
    let any = |texts: &[&str]| texts.iter().any(|text| reason.contains(text));
    if any(&[
        "source head changed from",
        "does not apply cleanly on top of",
        "is not mergeable:",
        "GitHub HTTP 405:",
        "GitHub HTTP 409:",
        "GitHub HTTP 5",
        ": HTTP 502: ",
        ": HTTP 503: ",
        ": HTTP 504: ",
        "GitHub request failed (transport or redirect)",
        "Publication interrupted by restart",
    ]) {
        vec![HoldRelease::NewHead, HoldRelease::TargetMoved]
    } else if any(&[
        "source identity, routing, title, state or draft status changed; evaluate again",
        "source checks changed; evaluate again",
        "target or context branch changed; evaluate again",
        "No forge checks were reported",
        "No successful forge checks",
        "current configuration no longer permits publication",
        "publishing bot changed",
    ]) {
        vec![HoldRelease::NewHead]
    } else {
        vec![]
    }
}
