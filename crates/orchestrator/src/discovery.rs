// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Opt-in, first-match PR assessment with frozen authority for separately handled publication.

use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::{
    DeliveryMode, PullRequestAction, PullRequestDiscovery, PullRequestHandler,
};
use artidezi_config::resolve::{
    PullRequestCiLog, PullRequestCiLogOutcome, PullRequestCiResult, PullRequestCiResultKind,
    PullRequestCiSnapshot, PullRequestCiState, PullRequestSource, TaskSelection,
};
use artidezi_contract::common::Outcome;
use artidezi_contract::job::InputRef;
use artidezi_forge::{
    CheckResultKind, ChecksState, CiLogOutcome, MergeRequestInfo, MergeRequestState,
};
use artidezi_git::bundle;
use artidezi_store::{model::*, new_id};
use chrono::{DateTime, Duration, Utc};
use git2::Repository;
use serde::Serialize;

use crate::{Orchestrator, OrchestratorError, Result};

#[derive(Debug, Default, Serialize)]
pub struct DiscoveryReport {
    pub queued: Vec<String>,
    pub errors: Vec<String>,
}

pub(crate) const EVALUATED_HEAD_STATUS: &str = "This head has already been evaluated";
pub(crate) const FOLLOW_UP_INTERVAL_SECONDS: i64 = 120;

struct DiscoveryTarget<'a> {
    project: &'a str,
    repository_id: &'a str,
    discovery: &'a PullRequestDiscovery,
}

pub fn source_of(task: &Task) -> Result<Option<PullRequestSource>> {
    let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
        .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
    Ok(policy.source_pull_request)
}

impl Orchestrator {
    fn source_head_evaluated(
        &self,
        task: &Task,
        head: &artidezi_contract::common::CommitSha,
    ) -> Result<bool> {
        Ok(
            source_of(task)?.is_some_and(|source| source.input.commit == *head)
                || self.equivalent_adaptation_head(task, head)?,
        )
    }

    /// Force bypasses only the polling interval, never matching, settling, deduplication or holds.
    pub fn scan_pull_requests(
        &mut self,
        project: Option<&str>,
        force: bool,
    ) -> Result<DiscoveryReport> {
        self.scan_pull_requests_at(project, force, Utc::now())
    }

    pub fn scan_pull_requests_at(
        &mut self,
        project: Option<&str>,
        force: bool,
        now: DateTime<Utc>,
    ) -> Result<DiscoveryReport> {
        if !force && self.db.discovery_paused()? {
            return Ok(DiscoveryReport::default());
        }
        if project.is_some_and(|p| !self.config.projects.contains_key(p)) {
            return Err(OrchestratorError::Invalid(
                "unknown discovery project".into(),
            ));
        }
        let projects: Vec<_> = self
            .config
            .projects
            .iter()
            .filter(|(name, _)| project.is_none_or(|p| p == name.as_str()))
            .filter_map(|(name, p)| p.pull_requests.clone().map(|d| (name.clone(), d)))
            .collect();
        let mut report = DiscoveryReport::default();
        // A project-scoped human scan must not mutate tracking records in another project.
        // Global scheduler/admin scans still retire records for removed projects.
        if project.is_none() {
            for mut source in self.db.list_pull_request_sources(None)? {
                if source.closed || self.config.projects.contains_key(&source.project) {
                    continue;
                }
                source.closed = true;
                source.paused = false;
                source.status = "Project removed; tracking retired".into();
                if let Some(task_id) = &source.last_task_id {
                    let task = self.db.get_task(task_id)?;
                    if !task.state.is_terminal() {
                        self.db
                            .set_task_state(task_id, TaskState::Closed, "discovery")?;
                        self.db.record_event(
                            Some(task_id),
                            None,
                            "discovery",
                            "assessment.closed_project",
                            serde_json::json!({"reason": "project removed"}),
                        )?;
                    }
                }
                self.db.save_pull_request_source(&source)?;
            }
        }
        for (name, discovery) in projects {
            if !self
                .db
                .discovery_scan_due(&name, discovery.poll_interval_seconds, now, force)?
            {
                continue;
            }
            for id in &discovery.repositories {
                if let Err(error) = self.scan_repository(&name, id, &discovery, now, &mut report) {
                    report.errors.push(format!("{name}/{id}: {error}"));
                }
                self.persist_forge_backoffs()?;
            }
        }
        Ok(report)
    }

    fn scan_repository(
        &mut self,
        project: &str,
        repository_id: &str,
        discovery: &PullRequestDiscovery,
        now: DateTime<Utc>,
        report: &mut DiscoveryReport,
    ) -> Result<()> {
        let repository = self.config.projects[project].repositories[repository_id].clone();
        let bot = self.config.forges[&repository.forge].bot_login.clone();
        let listed = self
            .forge(&repository.forge)?
            .list_open_merge_request_summaries(&repository.remote, None)?;
        let tracked = self.db.list_pull_request_sources(Some(project))?;
        let mut pulls = Vec::new();
        for listing in &listed {
            let summary = &listing.summary;
            if summary.author_login.eq_ignore_ascii_case(&bot)
                || summary.source_branch.starts_with("artidezi/")
            {
                continue;
            }
            let existing = tracked
                .iter()
                .find(|s| s.repository_id == repository_id && s.number == summary.number);
            let handler = discovery
                .handlers
                .iter()
                .find(|h| h.matches_pull_request(&summary.author_login, &summary.title));
            if existing.is_none()
                && (summary.target_branch != repository.branch
                    || handler.is_none_or(|h| h.action == PullRequestAction::Ignore))
            {
                continue;
            }
            if let Some(source) = existing
                && !source.closed
                && !source.paused
                && source.title == summary.title
                && source.observed_head == summary.head_commit.0
                && summary.target_branch == repository.branch
                && handler.is_some_and(|h| {
                    h.id == source.handler_id && h.action == PullRequestAction::Assess
                })
                && let Some(id) = &source.last_task_id
            {
                let task = self.db.get_task(id)?;
                if task.state == TaskState::Done
                    && self.source_head_evaluated(&task, &summary.head_commit)?
                {
                    if source.status != EVALUATED_HEAD_STATUS {
                        let mut updated = source.clone();
                        updated.status = EVALUATED_HEAD_STATUS.into();
                        self.db.save_pull_request_source(&updated)?;
                    }
                    continue;
                }
            }
            let detail = match &listing.details {
                Some(detail) => Ok(detail.clone()),
                None => self
                    .forge(&repository.forge)?
                    .get_merge_request(&repository.remote, summary.number),
            };
            match detail {
                Ok(pull) => pulls.push(pull),
                Err(error) => report.errors.push(format!(
                    "{project}/{repository_id} #{}: {error}",
                    summary.number
                )),
            }
        }
        // A missing previously open PR needs an individual read to confirm closure.
        // Keep reading closed PRs with active tasks until their lifecycle settles.
        // A closed PR with no active task is found again if it reopens in the open list.
        for source in tracked.iter().filter(|s| s.repository_id == repository_id) {
            if source.closed {
                let settled = match source.last_task_id.as_deref() {
                    Some(id) => self.db.get_task(id)?.state.is_terminal(),
                    None => true,
                };
                if settled {
                    continue;
                }
            }
            if !listed.iter().any(|p| p.summary.number == source.number) {
                match self
                    .forge(&source.forge)?
                    .get_merge_request(&source.remote, source.number)
                {
                    Ok(pull) => pulls.push(pull),
                    Err(error) => report.errors.push(format!("{}: {error}", source.id)),
                }
            }
        }
        pulls.sort_by_key(|p| p.number);
        for pull in pulls {
            if pull.author_login.eq_ignore_ascii_case(&bot)
                || pull.source_branch.starts_with("artidezi/")
            {
                continue;
            }
            let existing = tracked
                .iter()
                .find(|s| s.repository_id == repository_id && s.number == pull.number);
            self.process_observed_pull(
                DiscoveryTarget {
                    project,
                    repository_id,
                    discovery,
                },
                now,
                pull,
                existing.cloned(),
                report,
            )?;
        }
        Ok(())
    }

    fn process_observed_pull(
        &mut self,
        target: DiscoveryTarget<'_>,
        now: DateTime<Utc>,
        pull: MergeRequestInfo,
        existing: Option<TrackedPullRequest>,
        report: &mut DiscoveryReport,
    ) -> Result<()> {
        let DiscoveryTarget {
            project,
            repository_id,
            discovery,
        } = target;
        let repository = &self.config.projects[project].repositories[repository_id];
        let handler = discovery
            .handlers
            .iter()
            .find(|h| h.matches_pull_request(&pull.author_login, &pull.title));
        if existing.is_none()
            && (pull.state != MergeRequestState::Open
                || pull.target_branch != repository.branch
                || handler.is_none_or(|h| h.action == PullRequestAction::Ignore))
        {
            return Ok(());
        }
        let mut source = existing.unwrap_or_else(|| TrackedPullRequest {
            id: new_id("pr"),
            project: project.into(),
            repository_id: repository_id.into(),
            forge: repository.forge.clone(),
            remote: repository.remote.clone(),
            number: pull.number,
            url: pull.url.clone(),
            title: pull.title.clone(),
            closed: pull.state != MergeRequestState::Open,
            handler_id: handler.expect("matched source").id.clone(),
            observed_head: pull.head_commit.0.clone(),
            head_observed_at: now,
            last_task_id: None,
            runs: 0,
            paused: false,
            status: "observed".into(),
            follow_up: None,
        });
        let reopened_with_new_head = source.closed
            && pull.state == MergeRequestState::Open
            && source.observed_head != pull.head_commit.0;
        source.title = pull.title.clone();
        source.closed = pull.state != MergeRequestState::Open;
        if source.observed_head != pull.head_commit.0 {
            source.observed_head = pull.head_commit.0.clone();
            source.head_observed_at = now;
        }
        let result = (|| {
            if reopened_with_new_head {
                let prior_work_finished = match &source.last_task_id {
                    Some(id) => {
                        let task = self.db.get_task(id)?;
                        let finished = task.state.is_terminal()
                            && self
                                .db
                                .latest_attempt(id)?
                                .is_none_or(|attempt| attempt.state.is_terminal());
                        if finished {
                            // Include durable plans: a lost publication receipt can still hide
                            // an open adaptation that must be resolved before another assessment.
                            self.require_resolved_source_proposals(&task)?;
                        }
                        finished
                    }
                    None => true,
                };
                if prior_work_finished {
                    // The reopened revision starts a new budget; immutable task source references
                    // retain the previous lifecycle's evidence and publication history.
                    source.runs = 0;
                    source.paused = false;
                    source.last_task_id = None;
                }
            }
            self.consider_pull(&mut source, &pull, handler, discovery, now)
        })();
        match result {
            Ok(Some(task)) => report.queued.push(task.id),
            Ok(None) => self.db.save_pull_request_source(&source)?,
            Err(error) => {
                source.paused = !matches!(
                    error,
                    OrchestratorError::Forge(artidezi_forge::ForgeError::RetryAfter { .. })
                );
                source.status = format!("Preparation failed: {error}");
                if !source.paused {
                    source.follow_up = Some(PullRequestFollowUp {
                        at: now + Duration::seconds(FOLLOW_UP_INTERVAL_SECONDS),
                        reason: PullRequestFollowUpReason::Retry,
                    });
                }
                self.db.save_pull_request_source(&source)?;
                report.errors.push(format!("{}: {error}", source.id));
            }
        }
        Ok(())
    }

    /// Revisit only unresolved sources whose persisted deadline has passed. The repository-wide
    /// open listing remains on the configured poll interval.
    pub(crate) fn follow_up_pull_requests_at(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<DiscoveryReport> {
        if self.db.discovery_paused()? {
            return Ok(DiscoveryReport::default());
        }
        let mut report = DiscoveryReport::default();
        for mut source in self.db.list_pull_request_sources(None)? {
            let Some(follow_up) = source.follow_up.as_ref() else {
                continue;
            };
            if source.closed || source.paused || follow_up.at > now {
                continue;
            }
            let Some(project) = self.config.projects.get(&source.project) else {
                continue;
            };
            let Some(discovery) = project.pull_requests.clone() else {
                continue;
            };
            if !discovery.repositories.contains(&source.repository_id) {
                continue;
            }
            let Some(repository) = project.repositories.get(&source.repository_id) else {
                continue;
            };
            if source.forge != repository.forge || source.remote != repository.remote {
                source.paused = true;
                source.follow_up = None;
                source.status = "Watched repository identity changed; restore configuration or explicitly migrate tracking".into();
                self.db.save_pull_request_source(&source)?;
                continue;
            }
            if follow_up.reason == PullRequestFollowUpReason::Capacity {
                let active = self
                    .db
                    .list_tasks(Some(TaskState::Working))?
                    .iter()
                    .filter(|t| t.kind == TaskKind::PullRequest && t.project == source.project)
                    .count();
                if active >= discovery.max_active_tasks as usize {
                    continue;
                }
            }
            match self
                .forge(&source.forge)?
                .get_merge_request(&source.remote, source.number)
            {
                Ok(pull) => {
                    let project = source.project.clone();
                    let repository_id = source.repository_id.clone();
                    self.process_observed_pull(
                        DiscoveryTarget {
                            project: &project,
                            repository_id: &repository_id,
                            discovery: &discovery,
                        },
                        now,
                        pull,
                        Some(source),
                        &mut report,
                    )?
                }
                Err(error) => {
                    source.follow_up = Some(PullRequestFollowUp {
                        at: now + Duration::seconds(FOLLOW_UP_INTERVAL_SECONDS),
                        reason: PullRequestFollowUpReason::Retry,
                    });
                    self.db.save_pull_request_source(&source)?;
                    report.errors.push(format!("{}: {error}", source.id));
                }
            }
            self.persist_forge_backoffs()?;
        }
        Ok(report)
    }

    fn consider_pull(
        &mut self,
        source: &mut TrackedPullRequest,
        pull: &MergeRequestInfo,
        handler: Option<&PullRequestHandler>,
        discovery: &PullRequestDiscovery,
        now: DateTime<Utc>,
    ) -> Result<Option<Task>> {
        source.follow_up = None;
        if pull.state != MergeRequestState::Open {
            source.status = "Source PR is closed or merged".into();
            if let Some(id) = &source.last_task_id {
                let task = self.db.get_task(id)?;
                if task.state == TaskState::WaitingForCi {
                    // The scheduled delivery check reconciles an accepted approval/merge receipt
                    // before deciding whether closure needs attention. Discovery must not erase it.
                    return Ok(None);
                }
                let latest_attempt = self.db.latest_attempt(id)?;
                // A validated attempt waiting for CI may have just been converted to a human
                // decision by the delivery reconciler in this maintenance pass. Preserve that
                // state so it can explain why publication stopped; an older human hold should be
                // retired once its source PR disappears.
                let preserve_valid_human_hold = task.state == TaskState::NeedsHuman
                    && latest_attempt
                        .as_ref()
                        .is_some_and(|attempt| attempt.state == AttemptState::Valid)
                    && self
                        .db
                        .list_events(id)?
                        .iter()
                        .rev()
                        .find(|event| event.kind == "task.state_changed")
                        .is_some_and(|event| {
                            event.payload.get("from")
                                == Some(&serde_json::Value::String("waiting_for_ci".into()))
                        });
                if !task.state.is_terminal() && !preserve_valid_human_hold {
                    // Once the source PR is gone, an unfinished assessment can no longer produce
                    // useful work. Retire it automatically so stale NeedsHuman or AwaitingReview
                    // tasks do not remain in the operator's queue forever.
                    if let Some(attempt) = latest_attempt
                        && !attempt.state.is_terminal()
                    {
                        self.db.cancel_attempt(&attempt.id, "discovery")?;
                    }
                    self.db
                        .set_task_state(&task.id, TaskState::Closed, "discovery")?;
                    self.db.record_event(
                        Some(&task.id),
                        None,
                        "discovery",
                        "assessment.closed_source",
                        serde_json::json!({"reason": "source PR is closed or merged"}),
                    )?;
                }
            }
            return Ok(None);
        }
        if source.paused {
            return Ok(None);
        }
        let repository = &self.config.projects[&source.project].repositories[&source.repository_id];
        if source.forge != repository.forge || source.remote != repository.remote {
            source.paused = true;
            source.status = "Watched repository identity changed; restore configuration or explicitly migrate tracking".into();
            return Ok(None);
        }
        if pull.target_branch != repository.branch
            || handler
                .is_none_or(|h| h.id != source.handler_id || h.action == PullRequestAction::Ignore)
        {
            source.paused = true;
            source.status = "Routing changed; explicit resume required".into();
            return Ok(None);
        }
        let handler = handler.expect("checked handler");
        if let Some(id) = &source.last_task_id {
            let task = self.db.get_task(id)?;
            if task.state == TaskState::WaitingForCi {
                if source_of(&task)?.is_some_and(|s| s.input.commit != pull.head_commit) {
                    // The delivery reconciler checks frozen identity and publication plans
                    // before deciding whether this obsolete result can be retired safely.
                    source.status = "Source changed; waiting for delivery reconciliation".into();
                } else {
                    source.status = "Waiting for CI; delivery will continue automatically".into();
                }
                return Ok(None);
            }
            if let Some(attempt) = self.db.latest_attempt(id)?
                && !attempt.state.is_terminal()
            {
                if attempt.state == AttemptState::Queued
                    && source_of(&task)?.is_some_and(|s| s.input.commit != pull.head_commit)
                {
                    self.db.cancel_attempt(&attempt.id, "discovery")?;
                    self.db.set_task_state(id, TaskState::Closed, "discovery")?;
                    source.status =
                        "Queued input was superseded; waiting for the latest head".into();
                    return Ok(None);
                }
                source.status = "An evaluation is already queued or running".into();
                return Ok(None);
            }
            if task.state == TaskState::AwaitingReview {
                source.status = if !self.source_head_evaluated(&task, &pull.head_commit)? {
                    // The delivery reconciler closes obsolete adaptations or holds the task.
                    "Source changed; waiting for delivery reconciliation"
                } else {
                    "Published adaptations await review"
                }
                .into();
                return Ok(None);
            }
            if task.state == TaskState::NeedsHuman {
                source.paused = true;
                source.status = "The last evaluation needs human input".into();
                return Ok(None);
            }
            if task.state == TaskState::Working {
                self.db
                    .set_task_state(id, TaskState::NeedsHuman, "discovery")?;
                source.paused = true;
                source.status =
                    "Interrupted result processing; inspect the task before resuming".into();
                return Ok(None);
            }
            if task.state == TaskState::Done
                && self.source_head_evaluated(&task, &pull.head_commit)?
            {
                source.status = EVALUATED_HEAD_STATUS.into();
                return Ok(None);
            }
        }
        let settled_at =
            source.head_observed_at + Duration::seconds(i64::from(discovery.settle_seconds));
        if settled_at > now {
            source.status = "Waiting for the source head to settle".into();
            source.follow_up = Some(PullRequestFollowUp {
                at: settled_at,
                reason: PullRequestFollowUpReason::Settling,
            });
            return Ok(None);
        }
        if pull.draft {
            source.status = "Waiting for the PR to leave draft state".into();
            source.follow_up = Some(PullRequestFollowUp {
                at: now + Duration::seconds(FOLLOW_UP_INTERVAL_SECONDS),
                reason: PullRequestFollowUpReason::Draft,
            });
            return Ok(None);
        }
        if handler.wait_for_checks_to_finish && pull.checks == ChecksState::Pending {
            source.status = "Waiting for forge checks to finish".into();
            source.follow_up = Some(PullRequestFollowUp {
                at: now + Duration::seconds(FOLLOW_UP_INTERVAL_SECONDS),
                reason: PullRequestFollowUpReason::Checks,
            });
            return Ok(None);
        }
        if source.runs >= discovery.max_runs_per_pull_request {
            source.paused = true;
            source.status = "Automatic evaluation budget reached; explicit resume required".into();
            return Ok(None);
        }
        let active = self
            .db
            .list_tasks(Some(TaskState::Working))?
            .iter()
            .filter(|t| t.kind == TaskKind::PullRequest && t.project == source.project)
            .count();
        if active >= discovery.max_active_tasks as usize {
            source.status = "Waiting for project capacity".into();
            source.follow_up = Some(PullRequestFollowUp {
                at: now,
                reason: PullRequestFollowUpReason::Capacity,
            });
            return Ok(None);
        }
        self.create_pull_request_task(source, pull, handler, None)
            .map(Some)
    }

    fn create_pull_request_task(
        &mut self,
        source: &mut TrackedPullRequest,
        pull: &MergeRequestInfo,
        handler: &PullRequestHandler,
        feedback: Option<(&str, &str)>,
    ) -> Result<Task> {
        let mut policy = self.config.resolve_project(
            &source.project,
            &TaskSelection {
                agent_profile: handler.agent_profile.clone(),
                delivery_mode: None,
            },
            vec![
                Outcome::ApproveUntouched,
                Outcome::Proposal,
                Outcome::NeedsHuman,
                Outcome::Failed,
            ],
        )?;
        let publishing = handler.mode == artidezi_config::model::PullRequestMode::Publish;
        policy.delivery_mode = if publishing {
            DeliveryMode::MergeRequest
        } else {
            DeliveryMode::Shadow
        };
        policy.effects.merge_source_merge_request_on_safe_verdict &= publishing
            && handler.safe_verdict_action == artidezi_config::model::SafeVerdictAction::Merge;
        policy.effects.merge_adaptation_merge_requests &= publishing
            && handler.adaptation_action == artidezi_config::model::AdaptationAction::Merge;
        // Patches may be prepared and verified in shadow mode even when publishing is forbidden.
        policy.effects.publish_adaptation_merge_requests &= publishing;
        for repository in policy.repositories.values_mut() {
            repository.tagging = None;
        }
        let bases = self.resolve_bases(&policy)?;
        let repository = &policy.repositories[&source.repository_id];
        let forge = self.forge(&repository.forge)?;
        let remote_ref = forge.merge_request_ref(&repository.remote, pull.number)?;
        let cache = self.cache(&repository.forge, &repository.remote)?;
        let local_ref = format!("refs/artidezi/discovery/{}", source.id);
        cache.fetch(
            &forge.clone_url(&repository.remote),
            &[&format!("+{remote_ref}:{local_ref}")],
            forge.git_credentials(&repository.remote)?.as_ref(),
        )?;
        let git = Repository::open_bare(cache.path())?;
        if git.refname_to_id(&local_ref)?.to_string() != pull.head_commit.as_str() {
            return Err(OrchestratorError::Invalid(
                "PR head changed while snapshotting; rescan after inspection".into(),
            ));
        }
        let temp = self
            .artifacts
            .root()
            .join("tmp")
            .join(format!("{}.bundle", new_id("input")));
        bundle::create(cache.path(), &temp, &[&local_ref])?;
        let input = InputRef {
            name: "source-pr".into(),
            commit: pull.head_commit.clone(),
            bundle: self.artifacts.put_file(&temp)?,
        };
        let _ = std::fs::remove_file(temp);
        let ci_snapshot = if handler.ci_context.results {
            let results = pull.check_results.as_ref().ok_or_else(|| {
                OrchestratorError::Invalid(
                    "matched handler requests CI results, but the forge adapter did not provide them"
                        .into(),
                )
            })?;
            Some(PullRequestCiSnapshot {
                overall: match pull.checks {
                    ChecksState::Absent => PullRequestCiState::Absent,
                    ChecksState::Pending => PullRequestCiState::Pending,
                    ChecksState::Inconclusive => PullRequestCiState::Inconclusive,
                    ChecksState::Success => PullRequestCiState::Success,
                    ChecksState::Failure => PullRequestCiState::Failure,
                },
                results: results
                    .iter()
                    .map(|result| PullRequestCiResult {
                        kind: match result.kind {
                            CheckResultKind::CommitStatus => PullRequestCiResultKind::CommitStatus,
                            CheckResultKind::CheckRun => PullRequestCiResultKind::CheckRun,
                        },
                        name: result.name.clone(),
                        status: result.status.clone(),
                        conclusion: result.conclusion.clone(),
                    })
                    .collect(),
            })
        } else {
            None
        };
        let forge = self.forge(&repository.forge)?;
        let mut failed_names: Vec<String> = pull
            .check_results
            .iter()
            .flatten()
            .filter(|result| ci_snapshot.is_some() && forge.ci_log_candidate(result))
            .map(|result| result.name.clone())
            .collect();
        failed_names.sort();
        failed_names.dedup();
        let mut ci_logs = if handler.ci_context.logs
            && !failed_names.is_empty()
            && self.config.projects[&source.project]
                .pull_requests
                .as_ref()
                .is_some_and(|d| d.ci_log_context.permitted)
        {
            self.forge(&repository.forge)?
                .ci_job_logs(&repository.remote, &pull.head_commit, &failed_names)
                .into_iter()
                .filter(|log| failed_names.contains(&log.name))
                .take(2)
                .map(|log| {
                    let valid_excerpt = log.outcome == CiLogOutcome::Available
                        && log.excerpt.as_ref().is_some_and(|body| body.len() <= 3072)
                        && failed_names.contains(&log.name);
                    let outcome = if log.excerpt.is_some() && !valid_excerpt {
                        CiLogOutcome::TooLarge
                    } else if log.outcome == CiLogOutcome::Available && log.excerpt.is_none() {
                        CiLogOutcome::FetchFailed
                    } else {
                        log.outcome
                    };
                    let excerpt = log
                        .excerpt
                        .as_deref()
                        .filter(|_| valid_excerpt)
                        .map(|text| self.artifacts.put(text.as_bytes()))
                        .transpose()?;
                    Ok(PullRequestCiLog {
                        name: log.name,
                        job_id: log.job_id,
                        run_id: log.run_id,
                        run_attempt: log.run_attempt,
                        outcome: match outcome {
                            CiLogOutcome::Available => PullRequestCiLogOutcome::Available,
                            CiLogOutcome::Pending => PullRequestCiLogOutcome::Pending,
                            CiLogOutcome::UnsupportedProvider => {
                                PullRequestCiLogOutcome::UnsupportedProvider
                            }
                            CiLogOutcome::PermissionDenied => {
                                PullRequestCiLogOutcome::PermissionDenied
                            }
                            CiLogOutcome::MissingOrInaccessible => {
                                PullRequestCiLogOutcome::MissingOrInaccessible
                            }
                            CiLogOutcome::TooLarge => PullRequestCiLogOutcome::TooLarge,
                            CiLogOutcome::FetchFailed => PullRequestCiLogOutcome::FetchFailed,
                        },
                        captured_at: log.captured_at,
                        raw_bytes: log.raw_bytes,
                        sanitized_bytes: log.sanitized_bytes,
                        truncated: log.truncated,
                        excerpt,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        for (index, log) in ci_logs.iter_mut().enumerate() {
            let number = index + 1;
            let metadata = serde_json::to_string(log).expect("CI log metadata serializes");
            let section_bytes = format!(
                "CI job {number} from {} at {}: {metadata}\n",
                repository.remote, pull.head_commit
            )
            .len() as u64
                + log.excerpt.as_ref().map_or(0, |artifact| {
                    (crate::prompt::fence(
                        &format!("CI_LOG_{number}"),
                        &crate::prompt::fence_nonce(),
                        "",
                    )
                    .len()
                        + 2) as u64
                        + artifact.size
                });
            if section_bytes > 4 * 1024 {
                log.outcome = PullRequestCiLogOutcome::TooLarge;
                log.excerpt = None;
                log.truncated = false;
            }
        }
        policy.source_pull_request = Some(PullRequestSource {
            safe_verdict_action: if publishing {
                handler.safe_verdict_action
            } else {
                Default::default()
            },
            adaptation_action: if publishing {
                handler.adaptation_action
            } else {
                Default::default()
            },
            publisher_login: publishing
                .then(|| self.config.forges[&repository.forge].bot_login.clone()),
            tracking_id: source.id.clone(),
            repository_id: source.repository_id.clone(),
            number: pull.number,
            url: pull.url.clone(),
            title: handler.title_regex.as_ref().map(|_| pull.title.clone()),
            author: pull.author_login.clone(),
            source_branch: pull.source_branch.clone(),
            target_branch: pull.target_branch.clone(),
            handler_id: handler.id.clone(),
            handler_instructions: Some(handler.instructions.clone()),
            input,
            ci_snapshot,
            ci_logs,
        });
        let mut instructions = format!(
            "Assess source PR #{} in repository `{}`. Identify its own changes using the merge base of `refs/artidezi/inputs/source-pr` and the recorded target base, then assess integration with that target.\n\nSource title:\n{}\n\nSource description:\n{}",
            pull.number, source.repository_id, pull.title, pull.body
        );
        if let Some((feedback, _)) = feedback {
            instructions.push_str(&format!("\n\nHuman follow-up:\n{feedback}"));
        }
        let now = Utc::now();
        let task = Task {
            id: new_id("tsk"),
            number: 0,
            project: source.project.clone(),
            kind: TaskKind::PullRequest,
            created_at: now,
            updated_at: now,
            created_by: feedback
                .map(|(_, actor)| actor)
                .unwrap_or("discovery")
                .into(),
            instructions,
            delivery_mode: policy.delivery_mode,
            agent_profile: policy.agent_profile_name.clone(),
            policy_snapshot: serde_json::to_string(&policy)
                .map_err(|e| OrchestratorError::Invalid(e.to_string()))?,
            policy_digest: policy.digest(),
            state: TaskState::Working,
            repositories: bases,
        };
        let mut attempt = self.build_attempt(&task, &policy, 1, None)?;
        attempt.human_feedback = feedback.map(|(feedback, _)| feedback.to_owned());
        let mut updated = source.clone();
        updated.last_task_id = Some(task.id.clone());
        updated.runs += 1;
        updated.paused = false;
        updated.status = if publishing {
            "Queued for assessment and publication"
        } else {
            "Queued for shadow assessment"
        }
        .into();
        self.db.insert_discovery_task(&task, &attempt, &updated)?;
        if let Some(source_policy) = &policy.source_pull_request {
            let remote = &policy.repositories[&source_policy.repository_id].remote;
            for (index, log) in source_policy.ci_logs.iter().enumerate() {
                if matches!(
                    log.outcome,
                    PullRequestCiLogOutcome::PermissionDenied
                        | PullRequestCiLogOutcome::FetchFailed
                ) {
                    tracing::warn!(task_id = %task.id, handler = %source_policy.handler_id, job = %log.name, outcome = ?log.outcome, "configured CI job log is unavailable");
                }
                let number = index + 1;
                let metadata = serde_json::to_string(log).expect("CI log metadata serializes");
                let rendered_bytes = format!(
                    "CI job {number} from {remote} at {}: {metadata}\n",
                    source_policy.input.commit
                )
                .len() as u64
                    + log.excerpt.as_ref().map_or(0, |artifact| {
                        (crate::prompt::fence(
                            &format!("CI_LOG_{number}"),
                            &crate::prompt::fence_nonce(),
                            "",
                        )
                        .len()
                            + 2) as u64
                            + artifact.size
                    });
                self.db.record_event(
                    Some(&task.id),
                    None,
                    "discovery",
                    "ci_log.captured",
                    serde_json::json!({
                        "name": log.name,
                        "job_id": log.job_id,
                        "run_id": log.run_id,
                        "run_attempt": log.run_attempt,
                        "outcome": log.outcome,
                        "raw_bytes": log.raw_bytes,
                        "sanitized_bytes": log.sanitized_bytes,
                        "rendered_bytes": rendered_bytes,
                        "truncated": log.truncated,
                        "sha256": log.excerpt.as_ref().map(|a| &a.sha256),
                    }),
                )?;
            }
        }
        *source = updated;
        Ok(self.db.get_task(&task.id)?)
    }

    /// Stop queued automatic work; a currently leased attempt may finish without reactivation.
    pub fn pause_pull_request(&mut self, id: &str, actor: &str) -> Result<TrackedPullRequest> {
        let mut source = self.db.get_pull_request_source(id)?;
        source.paused = true;
        source.status = "Automatic assessments paused by a human".into();
        self.db.save_pull_request_source(&source)?;
        if let Some(task_id) = &source.last_task_id
            && self.db.get_task(task_id)?.state == TaskState::WaitingForCi
        {
            self.db
                .set_task_state(task_id, TaskState::NeedsHuman, actor)?;
        }
        if let Some(task_id) = &source.last_task_id
            && let Some(attempt) = self.db.latest_attempt(task_id)?
            && self.db.cancel_queued_attempt(&attempt.id, actor)?
        {
            self.db.set_task_state(task_id, TaskState::Closed, actor)?;
        }
        self.db.record_event(
            source.last_task_id.as_deref(),
            None,
            actor,
            "discovery.paused",
            serde_json::json!({"source": id}),
        )?;
        Ok(source)
    }

    /// A human explicitly renews the automatic budget. A scan still applies all eligibility gates.
    pub fn resume_pull_request(&mut self, id: &str, actor: &str) -> Result<TrackedPullRequest> {
        let mut source = self.db.get_pull_request_source(id)?;
        let project =
            self.config.projects.get(&source.project).ok_or_else(|| {
                OrchestratorError::Invalid("source project no longer exists".into())
            })?;
        let discovery = project.pull_requests.as_ref().ok_or_else(|| {
            OrchestratorError::Invalid("PR discovery is no longer enabled".into())
        })?;
        let repository = project
            .repositories
            .get(&source.repository_id)
            .filter(|r| {
                r.forge == source.forge
                    && r.remote == source.remote
                    && discovery.repositories.contains(&source.repository_id)
            })
            .ok_or_else(|| {
                OrchestratorError::Invalid("source repository is no longer watched".into())
            })?;
        let pull = self
            .forge(&source.forge)?
            .get_merge_request(&source.remote, source.number)?;
        if pull.state != MergeRequestState::Open || pull.target_branch != repository.branch {
            return Err(OrchestratorError::Invalid(
                "source PR is closed or targets an unwatched branch".into(),
            ));
        }
        source.handler_id = discovery
            .handlers
            .iter()
            .find(|h| h.matches_pull_request(&pull.author_login, &pull.title))
            .filter(|h| h.action == PullRequestAction::Assess)
            .ok_or_else(|| {
                OrchestratorError::Invalid("no assessment handler matches this source".into())
            })?
            .id
            .clone();
        if let Some(task_id) = &source.last_task_id {
            if self
                .db
                .latest_attempt(task_id)?
                .is_some_and(|a| !a.state.is_terminal())
            {
                return Err(OrchestratorError::Invalid(
                    "an evaluation is still active".into(),
                ));
            }
            let task = self.db.get_task(task_id)?;
            self.close_superseded_adaptations(&task)?;
            self.require_resolved_source_proposals(&task)?;
            if !task.state.is_terminal() {
                self.db.set_task_state(task_id, TaskState::Closed, actor)?;
            }
        }
        source.runs = 0;
        source.paused = false;
        // Retain task history through immutable source references, but allow explicit same-head retry.
        source.last_task_id = None;
        source.status = "Resumed by a human; waiting for the next eligible scan".into();
        self.db.save_pull_request_source(&source)?;
        self.db.record_event(
            None,
            None,
            actor,
            "discovery.resumed",
            serde_json::json!({"source": id}),
        )?;
        Ok(source)
    }

    pub(crate) fn revise_pull_request(
        &mut self,
        task: &Task,
        feedback: &str,
        actor: &str,
    ) -> Result<Attempt> {
        let frozen = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("PR task lacks source identity".into()))?;
        let source = self.db.get_pull_request_source(&frozen.tracking_id)?;
        if source.last_task_id.as_deref() != Some(&task.id) {
            return Err(OrchestratorError::Invalid(
                "a newer source evaluation exists; open its task instead".into(),
            ));
        }
        let discovery = self
            .config
            .projects
            .get(&source.project)
            .and_then(|p| p.pull_requests.as_ref())
            .ok_or_else(|| {
                OrchestratorError::Invalid("PR discovery is no longer enabled".into())
            })?;
        let pull = self
            .forge(&source.forge)?
            .get_merge_request(&source.remote, source.number)?;
        let handler = discovery
            .handlers
            .iter()
            .find(|h| h.matches_pull_request(&pull.author_login, &pull.title))
            .filter(|h| h.action == PullRequestAction::Assess)
            .cloned()
            .ok_or_else(|| {
                OrchestratorError::Invalid("no assessment handler matches this PR".into())
            })?;
        let repo = self.config.projects[&source.project]
            .repositories
            .get(&source.repository_id)
            .filter(|r| {
                r.forge == source.forge
                    && r.remote == source.remote
                    && discovery.repositories.contains(&source.repository_id)
            })
            .ok_or_else(|| {
                OrchestratorError::Invalid("source repository is no longer watched".into())
            })?;
        if pull.state != MergeRequestState::Open
            || pull.target_branch != repo.branch
            || pull
                .author_login
                .eq_ignore_ascii_case(&self.config.forges[&source.forge].bot_login)
            || pull.source_branch.starts_with("artidezi/")
        {
            return Err(OrchestratorError::Invalid(
                "source PR is no longer eligible".into(),
            ));
        }
        if self
            .db
            .latest_attempt(&task.id)?
            .is_some_and(|a| !a.state.is_terminal())
        {
            return Err(OrchestratorError::Invalid(
                "an evaluation is still active".into(),
            ));
        }
        self.require_resolved_source_proposals(task)?;
        let mut source = source;
        source.runs = 0;
        source.paused = false;
        source.handler_id = handler.id.clone();
        source.observed_head = pull.head_commit.0.clone();
        source.head_observed_at = Utc::now();
        let next =
            self.create_pull_request_task(&mut source, &pull, &handler, Some((feedback, actor)))?;
        if !task.state.is_terminal() {
            self.db.set_task_state(&task.id, TaskState::Closed, actor)?;
        }
        Ok(self
            .db
            .latest_attempt(&next.id)?
            .expect("atomically queued task"))
    }

    pub(crate) fn pause_task_source(&self, task: &Task, reason: &str) -> Result<()> {
        if let Some(source) = source_of(task)? {
            let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
            tracked.paused = true;
            tracked.status = reason.into();
            self.db.save_pull_request_source(&tracked)?;
        }
        Ok(())
    }

    pub(crate) fn record_shadow_result(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        result: &artidezi_contract::result::AgentResult,
    ) -> Result<()> {
        let source = source_of(task)?
            .ok_or_else(|| OrchestratorError::Invalid("shadow task lacks a PR source".into()))?;
        if task.kind != TaskKind::PullRequest || task.delivery_mode != DeliveryMode::Shadow {
            return Err(OrchestratorError::Invalid(
                "inconsistent shadow task policy".into(),
            ));
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let repository = &policy.repositories[&source.repository_id];
        let current = self
            .forge(&repository.forge)?
            .get_merge_request(&repository.remote, source.number)?;
        if current.head_commit != source.input.commit
            || current.state != MergeRequestState::Open
            || current.target_branch != source.target_branch
            || current.author_login != source.author
            || source
                .title
                .as_ref()
                .is_some_and(|title| current.title != *title)
            || current.draft
        {
            self.db.record_event(Some(&task.id), Some(&attempt.id), "discovery", "assessment.superseded",
                serde_json::json!({"evaluated_head": source.input.commit, "current_head": current.head_commit}))?;
            self.db
                .set_task_state(&task.id, TaskState::Closed, "discovery")?;
            return Ok(());
        }
        self.db.record_event(Some(&task.id), Some(&attempt.id), "discovery", "assessment.recorded",
            serde_json::json!({"outcome": result.outcome, "head": source.input.commit, "delivery": "shadow", "forge_writes": false}))?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if !tracked.paused {
            tracked.status = "Shadow assessment recorded; nothing was published".into();
        }
        self.db.save_pull_request_source(&tracked)?;
        match result.outcome {
            Outcome::ApproveUntouched | Outcome::Proposal => {
                self.db
                    .set_task_state(&task.id, TaskState::Done, "discovery")?
            }
            Outcome::NeedsHuman | Outcome::Failed => {
                self.needs_human(task, attempt, "shadow assessment needs human input")?
            }
        }
        Ok(())
    }
}
