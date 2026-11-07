// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Task creation, revisions and job building.

use std::collections::BTreeMap;

use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::TaskSelection;
use artidezi_contract::Job;
use artidezi_contract::attempt::AttemptRecord;
use artidezi_contract::common::{ArtifactRef, CommitSha, RepositoryId};
use artidezi_contract::job::{JobRepository, PreviousAttempt};
use artidezi_contract::result::AgentResult;
use artidezi_forge::MergeRequestState as ForgeMergeRequestState;
use artidezi_git::bundle;
use artidezi_store::model::*;
use artidezi_store::new_id;
use chrono::Utc;
use git2::Oid;

use crate::prompt::{self, PROMPT_TEMPLATE_VERSION, PreviousAttemptSummary};
use crate::{Orchestrator, OrchestratorError, Result};

/// Everything a worker needs for one attempt, with bundles as artifact references.
#[derive(Clone, Debug)]
pub struct BuiltJob {
    pub job: Job,
    pub job_artifact: ArtifactRef,
    pub task_document: ArtifactRef,
}

impl Orchestrator {
    /// Queues another attempt for an open adaptation proposal while retaining its task and PR.
    pub fn revise_proposal(
        &mut self,
        task_id: &str,
        feedback: &str,
        actor: &str,
    ) -> Result<Attempt> {
        let mut task = self.db.get_task(task_id)?;
        if task.kind != TaskKind::PullRequest || task.delivery_mode != DeliveryMode::MergeRequest {
            return Err(OrchestratorError::Invalid(
                "only pull-request adaptation tasks have proposals to revise".into(),
            ));
        }
        if self
            .db
            .latest_attempt(task_id)?
            .is_some_and(|a| !a.state.is_terminal())
        {
            return Err(OrchestratorError::Invalid(
                "an evaluation is still active".into(),
            ));
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        self.require_source_authority(&task, &policy)?;
        let source = crate::discovery::source_of(&task)?
            .ok_or_else(|| OrchestratorError::Invalid("task lacks source identity".into()))?;
        let repository = &policy.repositories[&source.repository_id];
        let current_source = self
            .forge(&repository.forge)?
            .get_merge_request(&repository.remote, source.number)?;
        if current_source.state != ForgeMergeRequestState::Open
            || current_source.head_commit != source.input.commit
        {
            return Err(OrchestratorError::Invalid(
                "source PR changed or is no longer open; evaluate it again instead".into(),
            ));
        }
        let proposals: Vec<_> = self
            .db
            .list_merge_requests(task_id)?
            .into_iter()
            .filter(|mr| mr.state == MergeRequestState::Open)
            .collect();
        if proposals.is_empty() {
            return Err(OrchestratorError::Invalid(
                "this task has no open adaptation proposal to revise".into(),
            ));
        }
        for proposal in proposals {
            let proposal_repo = policy
                .repositories
                .get(&proposal.repository_id)
                .ok_or_else(|| {
                    OrchestratorError::Invalid(
                        "adaptation proposal repository is no longer configured".into(),
                    )
                })?;
            let proposal_info = self
                .forge(&proposal.forge)?
                .get_merge_request(&proposal_repo.remote, proposal.number)
                .map_err(|_| {
                    OrchestratorError::Invalid("could not inspect the adaptation proposal".into())
                })?;
            if proposal_info.state != ForgeMergeRequestState::Open
                || proposal_info.source_branch != proposal.source_branch
                || proposal_info.target_branch != proposal_repo.branch
                || proposal_info.head_commit.as_str() != proposal.head_commit
            {
                return Err(OrchestratorError::Invalid(
                    "the adaptation proposal changed externally; review it manually".into(),
                ));
            }
        }
        task.repositories = self.resolve_bases(&policy)?;
        let number = self.db.list_attempts(task_id)?.len() as u32 + 1;
        let attempt = self.queue_attempt(&task, &policy, number, Some((feedback, actor)))?;
        self.db.set_task_state(task_id, TaskState::Working, actor)?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        tracked.status = "Proposal revision queued".into();
        self.db.save_pull_request_source(&tracked)?;
        Ok(attempt)
    }

    /// Creates a manual task from authenticated input, freezes its policy and base commits, and
    /// queues the first attempt.
    pub fn create_manual_task(
        &mut self,
        project: &str,
        instructions: &str,
        created_by: &str,
        selection: &TaskSelection,
    ) -> Result<Task> {
        let outcomes = self
            .config
            .projects
            .get(project)
            .ok_or_else(|| OrchestratorError::Invalid(format!("unknown project {project}")))?
            .manual_task_outcomes();
        let policy = self.config.resolve_project(project, selection, outcomes)?;
        // A project that disables publication may still run a manual task locally and
        // retain the resulting proposal for review. Published tasks keep the normal forge
        // write guard.
        if policy.effects.publish_adaptation_merge_requests {
            self.require_policy_writes(&policy)?;
        }
        let now = Utc::now();
        let mut task = Task {
            id: new_id("tsk"),
            number: 0,
            project: project.to_owned(),
            kind: TaskKind::Manual,
            created_at: now,
            updated_at: now,
            created_by: created_by.to_owned(),
            instructions: instructions.to_owned(),
            delivery_mode: policy.delivery_mode,
            agent_profile: policy.agent_profile_name.clone(),
            policy_snapshot: serde_json::to_string(&policy)
                .map_err(|e| OrchestratorError::Invalid(e.to_string()))?,
            policy_digest: policy.digest(),
            state: TaskState::Working,
            repositories: vec![],
        };
        task.repositories = self.resolve_bases(&policy)?;
        self.db.insert_task(&task)?;
        self.queue_attempt(&task, &policy, 1, None)?;
        Ok(self.db.get_task(&task.id)?)
    }

    /// Queues another attempt with authenticated human feedback. The base is re-resolved so the
    /// revision starts from the current target branch.
    pub fn request_revision(
        &mut self,
        task_id: &str,
        feedback: &str,
        actor: &str,
    ) -> Result<Attempt> {
        let mut task = self.db.get_task(task_id)?;
        if task.kind == TaskKind::PullRequest {
            return self.revise_pull_request(&task, feedback, actor);
        }
        if task.state.is_terminal() {
            return Err(OrchestratorError::Invalid(format!(
                "task {task_id} is {}",
                task.state.as_str()
            )));
        }
        if let Some(latest) = self.db.latest_attempt(task_id)?
            && !latest.state.is_terminal()
        {
            return Err(OrchestratorError::Invalid(format!(
                "attempt {} is still {}",
                latest.id,
                latest.state.as_str()
            )));
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        if policy.effects.publish_adaptation_merge_requests {
            self.require_policy_writes(&policy)?;
        }
        task.repositories = self.resolve_bases(&policy)?;
        let number = self.db.list_attempts(task_id)?.len() as u32 + 1;
        let attempt = self.queue_attempt(&task, &policy, number, Some((feedback, actor)))?;
        if task.state != TaskState::Working {
            self.db.set_task_state(task_id, TaskState::Working, actor)?;
        }
        Ok(attempt)
    }

    pub(crate) fn resolve_bases(
        &self,
        policy: &ResolvedProjectPolicy,
    ) -> Result<Vec<TaskRepository>> {
        let mut bases = vec![];
        for (id, repo) in &policy.repositories {
            let forge = self.forge(&repo.forge)?;
            let head = forge.branch_head(&repo.remote, &repo.branch)?;
            let cache = self.cache(&repo.forge, &repo.remote)?;
            let credentials = forge.git_credentials(&repo.remote)?;
            cache.fetch(
                &forge.clone_url(&repo.remote),
                &[&format!("+refs/heads/{0}:refs/heads/{0}", repo.branch)],
                credentials.as_ref(),
            )?;
            let oid = Oid::from_str(head.as_str())
                .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
            if !cache.has_commit(oid)
                || cache.resolve(&format!("refs/heads/{}", repo.branch))? != oid
            {
                return Err(OrchestratorError::Invalid(format!(
                    "forge reports {head} for {}@{} but the fetched branch differs; retry after the target settles",
                    repo.remote, repo.branch
                )));
            }
            bases.push(TaskRepository {
                repository_id: id.clone(),
                branch: repo.branch.clone(),
                base_commit: head.0,
            });
        }
        Ok(bases)
    }

    fn snapshot_bundle(
        &self,
        policy: &ResolvedProjectPolicy,
        task_repo: &TaskRepository,
    ) -> Result<ArtifactRef> {
        let repo = &policy.repositories[&task_repo.repository_id];
        let cache = self.cache(&repo.forge, &repo.remote)?;
        let oid = Oid::from_str(&task_repo.base_commit)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        // `git clone` only takes branches and tags from a bundle, so the snapshot is published under
        // the branch name itself, pinned to the recorded base.
        let refname = format!("refs/heads/{}", task_repo.branch);
        cache.set_ref(&refname, oid, "task base")?;
        let tmp = self
            .artifacts
            .root()
            .join("tmp")
            .join(format!("snapshot-{}.bundle", new_id("b")));
        bundle::create(cache.path(), &tmp, &[&refname])?;
        let artifact = self.artifacts.put_file(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(artifact)
    }

    fn queue_attempt(
        &mut self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        number: u32,
        revision: Option<(&str, &str)>,
    ) -> Result<Attempt> {
        let attempt = self.build_attempt(task, policy, number, revision)?;
        self.db.insert_attempt(&attempt)?;
        Ok(attempt)
    }

    pub(crate) fn build_attempt(
        &mut self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        number: u32,
        revision: Option<(&str, &str)>,
    ) -> Result<Attempt> {
        let attempt_id = new_id("att");

        // Previous attempt context for revisions.
        let mut previous_summary_storage: Option<(AttemptRecord, Option<AgentResult>, String)> =
            None;
        let mut previous_input: Option<PreviousAttempt> = None;
        if let Some((feedback, _)) = revision
            && let Some(prev) = self.db.latest_attempt(&task.id)?
            && let Some(record_ref) = &prev.attempt_record
        {
            let record: AttemptRecord = self.artifacts.get_json(record_ref)?;
            let result: Option<AgentResult> = prev
                .result
                .as_ref()
                .and_then(|r| self.artifacts.get_json(r).ok());
            let mut commit_bundles = BTreeMap::new();
            for repo in &record.repositories {
                if let Some(series) = &repo.series
                    && let Some(bundle) = &series.bundle
                {
                    commit_bundles.insert(repo.id.clone(), bundle.clone());
                }
            }
            previous_input = Some(PreviousAttempt {
                attempt_number: prev.number,
                attempt_document: record_ref.clone(),
                result_document: prev.result.clone(),
                commit_bundles,
            });
            previous_summary_storage = Some((record, result, feedback.to_owned()));
        }

        let mut repositories = vec![];
        for task_repo in &task.repositories {
            let resolved = &policy.repositories[&task_repo.repository_id];
            repositories.push(JobRepository {
                id: RepositoryId::new(task_repo.repository_id.clone()),
                access: resolved.access,
                branch: task_repo.branch.clone(),
                base_commit: CommitSha(task_repo.base_commit.clone()),
                snapshot_bundle: self.snapshot_bundle(policy, task_repo)?,
                input_refs: policy
                    .source_pull_request
                    .as_ref()
                    .filter(|source| source.repository_id == task_repo.repository_id)
                    .map(|source| vec![source.input.clone()])
                    .unwrap_or_default(),
                setup_recipe: resolved.setup_recipe.clone(),
                verification_recipe: resolved.verification_recipe.clone(),
                allowed_paths: resolved.allowed_paths.clone(),
                tagging: resolved.tagging.clone(),
            });
        }

        let mut job = Job {
            schema_version: artidezi_contract::job::JOB_SCHEMA_VERSION,
            attempt_id: attempt_id.as_str().into(),
            task_id: task.id.as_str().into(),
            attempt_number: number,
            prompt_template_version: PROMPT_TEMPLATE_VERSION,
            agent_profile: policy.agent_profile.clone(),
            worker_requirements: policy.worker_requirements.clone(),
            network: policy.network.clone(),
            limits: policy.limits,
            allowed_outcomes: policy.allowed_outcomes.clone(),
            repositories,
            task_document: ArtifactRef {
                sha256: "0".repeat(64),
                size: 0,
            },
            previous_attempt: previous_input,
        };

        let previous_summary = previous_summary_storage
            .as_ref()
            .map(|(record, result, feedback)| {
                let mut commit_messages = vec![];
                let mut verification_summary = vec![];
                for repo in &record.repositories {
                    if let Some(series) = &repo.series {
                        commit_messages.extend(
                            series
                                .commits
                                .iter()
                                .map(|c| format!("{}: {}", repo.id, c.message)),
                        );
                    }
                    for step in &repo.verification {
                        verification_summary.push(format!(
                            "{} / {}: {}",
                            repo.id,
                            step.name,
                            match (step.skipped, step.timed_out, step.exit_code) {
                                (true, _, _) => "skipped".to_owned(),
                                (_, true, _) => "timed out".to_owned(),
                                (_, _, Some(0)) => "passed".to_owned(),
                                (_, _, Some(code)) => format!("failed with exit code {code}"),
                                _ => "did not run".to_owned(),
                            }
                        ));
                    }
                }
                PreviousAttemptSummary {
                    number: record_number(record, &job),
                    result: result.as_ref(),
                    commit_messages,
                    verification_summary,
                    human_feedback: feedback,
                }
            })
            .or_else(|| {
                revision.map(|(feedback, _)| PreviousAttemptSummary {
                    number: number - 1,
                    result: None,
                    commit_messages: vec![],
                    verification_summary: vec![],
                    human_feedback: feedback,
                })
            });
        let nonce = prompt::fence_nonce();
        let mut ci_log_context = String::new();
        if let Some(source) = &policy.source_pull_request {
            let remote = &policy.repositories[&source.repository_id].remote;
            for (index, log) in source.ci_logs.iter().enumerate() {
                use std::fmt::Write as _;
                let metadata = serde_json::to_string(log)
                    .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
                writeln!(
                    ci_log_context,
                    "CI job {} from {} at {}: {metadata}",
                    index + 1,
                    remote,
                    source.input.commit
                )
                .unwrap();
                if let Some(artifact) = &log.excerpt {
                    let bytes = self.artifacts.get(artifact)?;
                    let excerpt = String::from_utf8(bytes).map_err(|_| {
                        OrchestratorError::Invalid("stored CI log excerpt is not UTF-8".into())
                    })?;
                    writeln!(
                        ci_log_context,
                        "{}\n",
                        prompt::fence(&format!("CI_LOG_{}", index + 1), &nonce, &excerpt)
                    )
                    .unwrap();
                }
            }
        }
        if ci_log_context.len() > 8 * 1024 {
            return Err(OrchestratorError::Invalid(
                "rendered CI log context exceeds 8 KiB".into(),
            ));
        }
        let document = prompt::render(
            policy,
            &job,
            &task.instructions,
            previous_summary.as_ref(),
            &ci_log_context,
            &nonce,
        );
        job.task_document = self.artifacts.put(document.as_bytes())?;

        let problems = artidezi_contract::validate::validate_job(&job);
        if !problems.is_empty() {
            return Err(OrchestratorError::Invalid(format!(
                "built job is invalid: {problems:?}"
            )));
        }
        let job_artifact = self.artifacts.put_json(&job)?;

        let now = Utc::now();
        let attempt = Attempt {
            id: attempt_id,
            task_id: task.id.clone(),
            number,
            state: AttemptState::Queued,
            created_at: now,
            updated_at: now,
            worker_pool: policy.worker_pool.clone(),
            job: job_artifact,
            human_feedback: revision.map(|(f, _)| f.to_owned()),
            not_before: None,
            attempt_record: None,
            result: None,
            outcome: None,
            validation_problems: vec![],
        };
        Ok(attempt)
    }

    pub fn load_job(&self, attempt: &Attempt) -> Result<Job> {
        Ok(self.artifacts.get_json(&attempt.job)?)
    }
}

fn record_number(_record: &AttemptRecord, job: &Job) -> u32 {
    job.previous_attempt
        .as_ref()
        .map(|p| p.attempt_number)
        .unwrap_or(job.attempt_number - 1)
}
