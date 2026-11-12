// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Applying a valid attempt's outcome: publication according to the task's delivery mode, or a
//! request for human attention. Repository code is never executed here: commits are re-created
//! from trees in the bare cache and the forge is re-checked before every push and merge.

use std::collections::BTreeSet;
use std::fmt::Write;

use artidezi_config::model::{DeliveryMode, ForgeProvider};
use artidezi_config::{ResolvedProjectPolicy, ResolvedRepository};
use artidezi_contract::Job;
use artidezi_contract::attempt::{AttemptRecord, RepositoryCollection};
use artidezi_contract::common::{CommitSha, Outcome, RepositoryAccess};
use artidezi_contract::result::AgentResult;
use artidezi_forge::{
    ChecksState, MergeMethod, MergeRequestInfo, MergeRequestState as ForgeMrState, NewMergeRequest,
};
use artidezi_git::publish::{recreate, recreate_rebased};
use artidezi_store::model::*;
use chrono::Utc;
use git2::Oid;

use crate::source_publish::{inline_code, limited, plain_note};
use crate::{Orchestrator, OrchestratorError, Result};

fn policy_of(task: &Task) -> Result<ResolvedProjectPolicy> {
    serde_json::from_str(&task.policy_snapshot)
        .map_err(|e| OrchestratorError::Invalid(e.to_string()))
}

fn source_branch(task_id: &str) -> String {
    format!("artidezi/{task_id}")
}

fn tag_proposal_for(result: &AgentResult, collection: &RepositoryCollection) -> Option<String> {
    result
        .repositories
        .iter()
        .find(|r| r.id == collection.id)
        .and_then(|r| r.tag_proposal.clone())
}

fn render_body(
    task: &Task,
    attempt: &Attempt,
    result: &AgentResult,
    collection: &RepositoryCollection,
) -> String {
    let mut body = String::new();
    writeln!(body, "{}\n", limited(&result.summary, 10_000)).unwrap();
    if let Some(hint) = &result.risk_hint {
        writeln!(
            body,
            "**Agent's risk hint (advisory, not verified):** {:?}. {}\n",
            hint.level,
            plain_note(&hint.rationale)
        )
        .unwrap();
    }
    if !result.evidence.is_empty() {
        writeln!(body, "**Evidence cited by the agent (unverified):**\n").unwrap();
        for e in &result.evidence {
            writeln!(
                body,
                "- {:?}: {} {}",
                e.kind,
                inline_code(&e.source, 500),
                plain_note(&e.note)
            )
            .unwrap();
        }
        writeln!(body).unwrap();
    }
    if !collection.verification.is_empty() {
        writeln!(body, "**Local verification:** passed\n").unwrap();
        for step in &collection.verification {
            let status = match (step.skipped, step.timed_out, step.exit_code) {
                (true, _, _) => "skipped".to_owned(),
                (_, true, _) => "timed out".to_owned(),
                (_, _, Some(0)) => "passed".to_owned(),
                (_, _, Some(code)) => format!("exit code {code}"),
                _ => "not run".to_owned(),
            };
            writeln!(body, "- `{}`: {status}", step.name).unwrap();
        }
        writeln!(body).unwrap();
    }
    if let Some(tag) = tag_proposal_for(result, collection) {
        writeln!(
            body,
            "**Proposed release tag:** `{tag}` (created by ArtiDezi after merge)\n"
        )
        .unwrap();
    }
    writeln!(
        body,
        "---\nArtiDezi task `{}`, attempt {} (`{}`). Revisions replace this branch; every attempt stays in ArtiDezi's audit log.",
        task.id, attempt.number, attempt.id
    )
    .unwrap();
    body
}

impl Orchestrator {
    /// Re-attempt a publication from a validated manual result without invoking another worker.
    pub(crate) fn retry_manual_publication(&mut self, task_id: &str, actor: &str) -> Result<Task> {
        let mut task = self.db.get_task(task_id)?;
        if task.kind != TaskKind::Manual
            || task.state != TaskState::NeedsHuman
            || !matches!(
                task.delivery_mode,
                DeliveryMode::MergeRequest | DeliveryMode::MergeRequestWithAutomerge
            )
        {
            return Err(OrchestratorError::Invalid(
                "only held manual merge-request tasks can retry publication".into(),
            ));
        }
        let attempt = self
            .db
            .latest_attempt(task_id)?
            .ok_or_else(|| OrchestratorError::Invalid("missing attempt".into()))?;
        if attempt.state != AttemptState::Valid {
            return Err(OrchestratorError::Invalid(
                "retry requires a validated result".into(),
            ));
        }
        let record: AttemptRecord = self.artifacts.get_json(
            attempt
                .attempt_record
                .as_ref()
                .ok_or_else(|| OrchestratorError::Invalid("missing record".into()))?,
        )?;
        let result: AgentResult = self.artifacts.get_json(
            attempt
                .result
                .as_ref()
                .ok_or_else(|| OrchestratorError::Invalid("missing result".into()))?,
        )?;
        if result.outcome != Outcome::Proposal {
            return Err(OrchestratorError::Invalid(
                "agent needs a new decision, not a publication retry".into(),
            ));
        }
        let job = self.load_job(&attempt)?;
        self.db.record_event(
            Some(task_id),
            Some(&attempt.id),
            actor,
            "publication.retry_requested",
            serde_json::json!({}),
        )?;
        self.db.set_task_state(task_id, TaskState::Working, actor)?;
        task.state = TaskState::Working;
        if let Err(error) = self.publish_proposal(&task, &attempt, &job, &record, &result) {
            self.needs_human(
                &task,
                &attempt,
                &format!("publication retry failed: {error}"),
            )?;
        }
        Ok(self.db.get_task(task_id)?)
    }

    /// Check frozen provider identity and the currently installed adapter before any write,
    /// including Git pushes which do not pass through the forge's HTTP implementation.
    pub(crate) fn require_repository_write(&self, resolved: &ResolvedRepository) -> Result<()> {
        self.require_repository_write_with_github(resolved, false)
    }

    fn require_repository_write_with_github(
        &self,
        resolved: &ResolvedRepository,
        allow_github: bool,
    ) -> Result<()> {
        if resolved.access != RepositoryAccess::ReadWrite
            || (!allow_github && resolved.forge_provider == ForgeProvider::Github)
            || self
                .config
                .forges
                .get(&resolved.forge)
                .is_none_or(|f| !allow_github && f.provider == ForgeProvider::Github)
            || !self.forge(&resolved.forge)?.writes_allowed()
        {
            return Err(OrchestratorError::Invalid(format!(
                "repository {}: forge is read-only; manual publication is unavailable, use Incoming changes for dry-run evaluation",
                resolved.remote
            )));
        }
        Ok(())
    }

    pub(crate) fn require_policy_writes(&self, policy: &ResolvedProjectPolicy) -> Result<()> {
        for repository in policy
            .repositories
            .values()
            .filter(|r| r.access == RepositoryAccess::ReadWrite)
        {
            self.require_repository_write_with_github(
                repository,
                policy.effects.allow_manual_github_publication,
            )?;
        }
        Ok(())
    }

    pub(crate) fn apply_outcome(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        job: &Job,
        record: &AttemptRecord,
        result: &AgentResult,
    ) -> Result<()> {
        if task.kind == TaskKind::PullRequest {
            return if task.delivery_mode == DeliveryMode::Shadow {
                self.record_shadow_result(task, attempt, result)
            } else {
                self.publish_source_result(task, attempt, job, record, result)
            };
        }
        if task.delivery_mode == DeliveryMode::Shadow {
            return Err(OrchestratorError::Invalid(
                "manual tasks cannot use dry run".into(),
            ));
        }
        match result.outcome {
            Outcome::Proposal => self.publish_proposal(task, attempt, job, record, result),
            Outcome::ApproveUntouched => {
                // Renovate evaluation lands with the discovery poller; until then this outcome is
                // never allowed by a job, so reaching here means a human must look.
                self.needs_human(
                    task,
                    attempt,
                    "approve_untouched is not yet wired to a merge effect",
                )
            }
            Outcome::NeedsHuman => self.needs_human(task, attempt, "agent asked for human input"),
            Outcome::Failed => self.needs_human(task, attempt, "agent reported failure"),
        }
    }

    pub(crate) fn needs_human(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        reason: &str,
    ) -> Result<()> {
        self.pause_task_source(task, reason)?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "task.needs_human",
            serde_json::json!({"reason": reason}),
        )?;
        if task.state != TaskState::NeedsHuman && !task.state.is_terminal() {
            self.db
                .set_task_state(&task.id, TaskState::NeedsHuman, "orchestrator")?;
        }
        Ok(())
    }

    /// Re-creates the series with the bot identity in the cache and returns the new head plus the
    /// cache ref that points at it.
    pub(crate) fn recreate_in_cache(
        &self,
        resolved: &ResolvedRepository,
        collection: &RepositoryCollection,
        attempt_id: &str,
    ) -> Result<(CommitSha, String)> {
        let cache = self.cache(&resolved.forge, &resolved.remote)?;
        let series = collection.series.as_ref().expect("validated series");
        let base = Oid::from_str(collection.base_commit.as_str())
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let oids: Vec<Oid> = series
            .commits
            .iter()
            .map(|c| {
                Oid::from_str(c.sha.as_str()).map_err(|e| OrchestratorError::Invalid(e.to_string()))
            })
            .collect::<Result<_>>()?;
        let created = recreate(cache.repo(), base, &oids, &self.bot)?;
        let head = *created.last().expect("non-empty series");
        let refname = format!("refs/artidezi/publish/{attempt_id}/{}", collection.id);
        cache.set_ref(&refname, head, "publish")?;
        Ok((CommitSha(head.to_string()), refname))
    }

    pub(crate) fn recreate_in_cache_rebased(
        &self,
        resolved: &ResolvedRepository,
        collection: &RepositoryCollection,
        attempt_id: &str,
    ) -> Result<(CommitSha, String)> {
        let cache = self.cache(&resolved.forge, &resolved.remote)?;
        let forge = self.forge(&resolved.forge)?;
        let series = collection.series.as_ref().expect("validated series");
        let base = forge.branch_head(&resolved.remote, &resolved.branch)?;
        cache.fetch(
            &forge.clone_url(&resolved.remote),
            &[&format!("+refs/heads/{0}:refs/heads/{0}", resolved.branch)],
            forge.git_credentials(&resolved.remote)?.as_ref(),
        )?;
        let base =
            Oid::from_str(base.as_str()).map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let oids: Vec<Oid> = series
            .commits
            .iter()
            .map(|c| {
                Oid::from_str(c.sha.as_str()).map_err(|e| OrchestratorError::Invalid(e.to_string()))
            })
            .collect::<Result<_>>()?;
        let created = recreate_rebased(cache.repo(), base, &oids, &self.bot)?;
        let head = *created.last().expect("non-empty series");
        let refname = format!("refs/artidezi/publish/{attempt_id}/{}", collection.id);
        cache.set_ref(&refname, head, "publish rebased")?;
        Ok((CommitSha(head.to_string()), refname))
    }

    fn push(&self, resolved: &ResolvedRepository, refspec: &str, expected_head: Oid) -> Result<()> {
        self.require_repository_write(resolved)?;
        self.push_after_check(resolved, refspec, expected_head)
    }

    fn push_after_check(
        &self,
        resolved: &ResolvedRepository,
        refspec: &str,
        expected_head: Oid,
    ) -> Result<()> {
        let forge = self.forge(&resolved.forge)?;
        let credentials = forge.git_push_credentials(&resolved.remote)?;
        let cache = self.cache(&resolved.forge, &resolved.remote)?;
        cache.push_with_lease(
            &forge.clone_url(&resolved.remote),
            refspec,
            credentials.as_ref(),
            expected_head,
        )?;
        Ok(())
    }

    fn push_with_github(
        &self,
        resolved: &ResolvedRepository,
        refspec: &str,
        expected_head: Oid,
        allow_github: bool,
        protected: bool,
    ) -> Result<()> {
        self.require_repository_write_with_github(resolved, allow_github)?;
        if !protected {
            return self.push_after_check(resolved, refspec, expected_head);
        }
        let forge = self.forge(&resolved.forge)?;
        let credentials = forge.git_push_workflow_credentials(&resolved.remote)?;
        self.cache(&resolved.forge, &resolved.remote)?
            .push_with_lease(
                &forge.clone_url(&resolved.remote),
                refspec,
                credentials.as_ref(),
                expected_head,
            )?;
        Ok(())
    }

    fn publish_proposal(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        _job: &Job,
        record: &AttemptRecord,
        result: &AgentResult,
    ) -> Result<()> {
        let policy = policy_of(task)?;
        if !policy.effects.publish_adaptation_merge_requests {
            return self.needs_human(
                task,
                attempt,
                "project does not permit publishing agent-authored changes",
            );
        }
        self.require_policy_writes(&policy)?;
        let changed: Vec<&RepositoryCollection> = record
            .repositories
            .iter()
            .filter(|c| {
                result
                    .repositories
                    .iter()
                    .any(|r| r.id == c.id && r.changed)
            })
            .collect();
        let protected = self.protected_repositories(&policy, record, result)?;
        if !protected.is_empty() && !self.workflow_publication_authorized(task, attempt)? {
            return self.needs_human(
                task,
                attempt,
                "Protected CI path changes require task-specific human authorization; publication withheld",
            );
        }

        match task.delivery_mode {
            DeliveryMode::Shadow => Err(OrchestratorError::Invalid(
                "shadow tasks cannot publish".into(),
            )),
            DeliveryMode::MergeRequest | DeliveryMode::MergeRequestWithAutomerge => {
                // Check every previous proposal before updating any of them. Publication across
                // forges is not atomic, but an already-known conflict must not cause a partial
                // revision. Repeat the checks immediately before each individual push below.
                for mr in self.db.list_merge_requests(&task.id)? {
                    let resolved = &policy.repositories[&mr.repository_id];
                    if !self.check_proposal_revision(task, attempt, resolved, &mr)? {
                        return Ok(());
                    }
                }
                for collection in &changed {
                    let protected = protected.contains(collection.id.as_str());
                    if !self.publish_merge_request(
                        task, attempt, &policy, result, collection, protected,
                    )? {
                        return Ok(());
                    }
                }
                if task.state != TaskState::AwaitingReview {
                    self.db
                        .set_task_state(&task.id, TaskState::AwaitingReview, "orchestrator")?;
                }
                Ok(())
            }
            DeliveryMode::DirectPushFastForward => {
                let [collection] = changed.as_slice() else {
                    return self.needs_human(
                        task,
                        attempt,
                        "direct push requires exactly one changed repository",
                    );
                };
                self.direct_push(task, attempt, &policy, result, collection)
            }
        }
    }

    /// Heads this task planned to push. A crash can leave one on the forge without a receipt.
    fn planned_manual_heads(&self, task_id: &str) -> Result<BTreeSet<String>> {
        Ok(self
            .db
            .list_events(task_id)?
            .iter()
            .filter(|event| event.kind == "manual_publication.planned")
            .filter_map(|event| event.payload["head"].as_str().map(str::to_owned))
            .collect())
    }

    /// Recorded before any push, so that a retry reuses the same commits and recognizes them.
    fn plan_manual_publication(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        resolved: &ResolvedRepository,
        collection: &RepositoryCollection,
    ) -> Result<(CommitSha, String)> {
        let previous = self.db.list_events(&task.id)?.into_iter().find(|event| {
            event.kind == "manual_publication.planned"
                && event.attempt_id.as_deref() == Some(&attempt.id)
                && event.payload["repository"] == collection.id.as_str()
        });
        if let Some(event) = previous {
            let field = |name: &str| event.payload[name].as_str().map(str::to_owned);
            if let (Some(head), Some(cache_ref)) = (field("head"), field("cache_ref")) {
                return Ok((CommitSha(head), cache_ref));
            }
        }
        let (head, cache_ref) = self.recreate_in_cache(resolved, collection, &attempt.id)?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "manual_publication.planned",
            serde_json::json!({"repository": collection.id, "head": head, "cache_ref": cache_ref}),
        )?;
        Ok((head, cache_ref))
    }

    fn check_proposal_revision(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        resolved: &ResolvedRepository,
        mr: &PublishedMergeRequest,
    ) -> Result<bool> {
        let forge = self.forge(&resolved.forge)?;
        let info = forge.get_merge_request(&resolved.remote, mr.number)?;
        let reason = if info.state != ForgeMrState::Open {
            Some("proposal was closed or merged; not rewriting")
        } else if info.source_branch != source_branch(&task.id)
            || info.target_branch != resolved.branch
        {
            Some("proposal branches differ from the recorded task; not rewriting")
        } else {
            let actual = forge.branch_head(&resolved.remote, &mr.source_branch)?;
            let known = self.planned_manual_heads(&task.id)?;
            if actual.as_str() != mr.head_commit && !known.contains(actual.as_str()) {
                Some("proposal branch was changed by someone else; not rewriting")
            } else {
                None
            }
        };
        if let Some(reason) = reason {
            self.needs_human(task, attempt, &format!("{}: {reason}", mr.repository_id))?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Returns false when publication was withheld and the task was routed to a human.
    fn publish_merge_request(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
        result: &AgentResult,
        collection: &RepositoryCollection,
        protected: bool,
    ) -> Result<bool> {
        let resolved = policy.repositories[collection.id.as_str()].clone();
        self.require_repository_write_with_github(
            &resolved,
            policy.effects.allow_manual_github_publication,
        )?;
        let branch = source_branch(&task.id);
        let existing = self
            .db
            .get_merge_request(&task.id, collection.id.as_str())?;

        if let Some(mr) = &existing
            && !self.check_proposal_revision(task, attempt, &resolved, mr)?
        {
            return Ok(false);
        }

        let (new_head, publish_ref) =
            self.plan_manual_publication(task, attempt, &resolved, collection)?;
        let remote_head = self
            .forge(&resolved.forge)?
            .branch_head_optional(&resolved.remote, &branch)?;
        if remote_head.as_ref() != Some(&new_head) {
            let known = self.planned_manual_heads(&task.id)?;
            let expected_head = match &remote_head {
                None => Oid::ZERO_SHA1,
                Some(head)
                    if known.contains(head.as_str())
                        || existing.as_ref().is_some_and(|mr| mr.head_commit == head.0) =>
                {
                    Oid::from_str(head.as_str())
                        .map_err(|e| OrchestratorError::Invalid(e.to_string()))?
                }
                Some(_) => {
                    self.needs_human(
                        task,
                        attempt,
                        &format!(
                            "{}: proposal branch was changed by someone else; not rewriting",
                            collection.id
                        ),
                    )?;
                    return Ok(false);
                }
            };
            self.push_with_github(
                &resolved,
                &format!("+{publish_ref}:refs/heads/{branch}"),
                expected_head,
                policy.effects.allow_manual_github_publication,
                protected,
            )?;
        }

        let title = collection
            .series
            .as_ref()
            .and_then(|s| s.commits.first())
            .and_then(|c| c.message.lines().next())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ArtiDezi: {}", task.id));
        let body = render_body(task, attempt, result, collection);
        let info: MergeRequestInfo = {
            let forge = self.forge(&resolved.forge)?;
            match &existing {
                Some(mr) => {
                    forge.update_merge_request(&resolved.remote, mr.number, &title, &body)?;
                    forge.get_merge_request(&resolved.remote, mr.number)?
                }
                None => {
                    let open: Vec<_> = forge
                        .proposals_for_branch(&resolved.remote, &branch)?
                        .into_iter()
                        .filter(|info| info.state == ForgeMrState::Open)
                        .collect();
                    match open.as_slice() {
                        [] => forge.create_merge_request(
                            &resolved.remote,
                            &NewMergeRequest {
                                title,
                                body,
                                source_branch: branch.clone(),
                                target_branch: resolved.branch.clone(),
                                draft: policy.draft_merge_requests,
                            },
                        )?,
                        [info]
                            if info.author_login
                                == self.config.forges[&resolved.forge].bot_login =>
                        {
                            forge.update_merge_request(
                                &resolved.remote,
                                info.number,
                                &title,
                                &body,
                            )?;
                            forge.get_merge_request(&resolved.remote, info.number)?
                        }
                        _ => {
                            return Err(OrchestratorError::Invalid(format!(
                                "{}: another proposal already uses branch {branch}",
                                collection.id
                            )));
                        }
                    }
                }
            }
        };
        self.db.upsert_merge_request(&PublishedMergeRequest {
            task_id: task.id.clone(),
            repository_id: collection.id.to_string(),
            forge: resolved.forge.clone(),
            number: info.number,
            url: info.url.clone(),
            source_branch: branch,
            head_commit: new_head.0.clone(),
            state: MergeRequestState::Open,
            updated_at: Utc::now(),
        })?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "merge_request.published",
            serde_json::json!({"repository": collection.id, "number": info.number, "url": info.url, "head": new_head, "revision": existing.is_some()}),
        )?;
        Ok(true)
    }

    fn direct_push(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
        result: &AgentResult,
        collection: &RepositoryCollection,
    ) -> Result<()> {
        let resolved = policy.repositories[collection.id.as_str()].clone();
        let actual = self
            .forge(&resolved.forge)?
            .branch_head(&resolved.remote, &resolved.branch)?;
        if actual != collection.base_commit {
            return self.needs_human(
                task,
                attempt,
                "target branch moved since the task base was recorded; not pushing",
            );
        }
        let (new_head, publish_ref) = self.recreate_in_cache(&resolved, collection, &attempt.id)?;
        self.push(
            &resolved,
            &format!("{publish_ref}:refs/heads/{}", resolved.branch),
            Oid::from_str(collection.base_commit.as_str())
                .map_err(|e| OrchestratorError::Invalid(e.to_string()))?,
        )?;
        self.db.record_event(
            Some(&task.id),
            Some(&attempt.id),
            "orchestrator",
            "branch.pushed",
            serde_json::json!({"repository": collection.id, "branch": resolved.branch, "head": new_head}),
        )?;
        self.create_tag_if_proposed(attempt, &resolved, result, collection, &new_head, false)?;
        self.db
            .set_task_state(&task.id, TaskState::Done, "orchestrator")?;
        Ok(())
    }

    fn create_tag_if_proposed(
        &mut self,
        attempt: &Attempt,
        resolved: &ResolvedRepository,
        result: &AgentResult,
        collection: &RepositoryCollection,
        commit: &CommitSha,
        allow_github: bool,
    ) -> Result<()> {
        let Some(tag) = tag_proposal_for(result, collection) else {
            return Ok(());
        };
        let already_recorded = self.db.list_events(&attempt.task_id)?.iter().any(|event| {
            event.kind == "tag.created"
                && event.payload["repository"] == collection.id.as_str()
                && event.payload["tag"] == tag.as_str()
        });
        if already_recorded {
            return Ok(());
        }
        self.require_repository_write_with_github(resolved, allow_github)?;
        let forge = self.forge(&resolved.forge)?;
        match forge.tag_commit(&resolved.remote, &tag)? {
            None => forge.create_tag(&resolved.remote, &tag, commit)?,
            Some(existing) if &existing == commit => {}
            Some(existing) => {
                return Err(OrchestratorError::Invalid(format!(
                    "tag {tag} already points at {existing}, not {commit}"
                )));
            }
        }
        self.db.record_event(
            Some(&attempt.task_id),
            Some(&attempt.id),
            "orchestrator",
            "tag.created",
            serde_json::json!({"repository": collection.id, "tag": tag, "commit": commit}),
        )?;
        Ok(())
    }

    /// Checks published merge requests against the forge: records merges, performs automerge when
    /// the task allows it and checks are green, and creates approved tags.
    pub fn reconcile_task(&mut self, task_id: &str) -> Result<TaskState> {
        let task = self.db.get_task(task_id)?;
        if task.state == TaskState::WaitingForCi {
            return self.resume_source_ci(&task, Utc::now());
        }
        if task.state != TaskState::AwaitingReview {
            return Ok(task.state);
        }
        let policy = policy_of(&task)?;
        if task.kind == TaskKind::PullRequest && task.delivery_mode == DeliveryMode::MergeRequest {
            return self.reconcile_source_task(&task, &policy);
        } else {
            self.require_policy_writes(&policy)?;
        }
        if task.delivery_mode == DeliveryMode::Shadow {
            return Err(OrchestratorError::Invalid(
                "dry-run tasks cannot reconcile publication".into(),
            ));
        }
        let Some(attempt) = self.db.latest_attempt(task_id)? else {
            return Ok(task.state);
        };
        let result: Option<AgentResult> = attempt
            .result
            .as_ref()
            .and_then(|r| self.artifacts.get_json(r).ok());
        let record: Option<AttemptRecord> = attempt
            .attempt_record
            .as_ref()
            .and_then(|r| self.artifacts.get_json(r).ok());
        let collection_for = |repository_id: &str| -> Option<RepositoryCollection> {
            record.as_ref().and_then(|r| {
                r.repositories
                    .iter()
                    .find(|c| c.id.as_str() == repository_id)
                    .cloned()
            })
        };

        let mut all_merged = true;
        for mut mr in self.db.list_merge_requests(task_id)? {
            let resolved = policy.repositories[mr.repository_id.as_str()].clone();
            let info = self
                .forge(&resolved.forge)?
                .get_merge_request(&resolved.remote, mr.number)?;
            match info.state {
                ForgeMrState::Merged => {
                    if mr.state != MergeRequestState::Merged {
                        mr.state = MergeRequestState::Merged;
                        mr.updated_at = Utc::now();
                        self.db.upsert_merge_request(&mr)?;
                    }
                    self.cleanup_proposal_branch(
                        &task,
                        Some(&attempt.id),
                        &policy,
                        &mr,
                        Utc::now(),
                    );
                    // Runs on every pass until recorded, so a failed tag is retried.
                    if let (Some(result), Some(collection)) =
                        (&result, collection_for(&mr.repository_id))
                        && tag_proposal_for(result, &collection).is_some()
                    {
                        let commit = info.merge_commit.as_ref().ok_or_else(|| {
                            OrchestratorError::Invalid(format!(
                                "forge reported no merge commit for {} #{}",
                                mr.repository_id, mr.number
                            ))
                        })?;
                        self.create_tag_if_proposed(
                            &attempt,
                            &resolved,
                            result,
                            &collection,
                            commit,
                            policy.effects.allow_manual_github_publication,
                        )?;
                    }
                }
                ForgeMrState::Closed => {
                    mr.state = MergeRequestState::Closed;
                    mr.updated_at = Utc::now();
                    self.db.upsert_merge_request(&mr)?;
                    self.db
                        .set_task_state(task_id, TaskState::Closed, "forge")?;
                    return Ok(TaskState::Closed);
                }
                ForgeMrState::Open => {
                    let automerge = task.delivery_mode == DeliveryMode::MergeRequestWithAutomerge
                        && info.checks == ChecksState::Success
                        && info.head_commit.as_str() == mr.head_commit;
                    if !automerge {
                        all_merged = false;
                        continue;
                    }
                    let merged = self.forge(&resolved.forge)?.merge(
                        &resolved.remote,
                        mr.number,
                        &info.head_commit,
                        MergeMethod::Rebase,
                    )?;
                    mr.state = MergeRequestState::Merged;
                    mr.updated_at = Utc::now();
                    self.db.upsert_merge_request(&mr)?;
                    self.db.record_event(
                        Some(task_id),
                        Some(&attempt.id),
                        "orchestrator",
                        "merge_request.automerged",
                        serde_json::json!({"repository": mr.repository_id, "number": mr.number, "commit": merged}),
                    )?;
                    self.cleanup_proposal_branch(
                        &task,
                        Some(&attempt.id),
                        &policy,
                        &mr,
                        Utc::now(),
                    );
                    if let (Some(result), Some(collection)) =
                        (&result, collection_for(&mr.repository_id))
                    {
                        self.create_tag_if_proposed(
                            &attempt,
                            &resolved,
                            result,
                            &collection,
                            &merged,
                            policy.effects.allow_manual_github_publication,
                        )?;
                    }
                }
            }
        }
        let merge_requests = self.db.list_merge_requests(task_id)?;
        if all_merged
            && !merge_requests.is_empty()
            && merge_requests
                .iter()
                .all(|m| m.state == MergeRequestState::Merged)
        {
            self.db
                .set_task_state(task_id, TaskState::Done, "orchestrator")?;
            return Ok(TaskState::Done);
        }
        Ok(TaskState::AwaitingReview)
    }
}
