// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Best-effort cleanup of superseded or merged owned proposals. Merged receipts atomically queue
//! bounded recovery independent of task state. Every try retains the original deletion lease.

use artidezi_config::{ResolvedProjectPolicy, resolve::ResolvedRepository};
use artidezi_contract::common::RepositoryAccess;
use artidezi_forge::MergeRequestState as RemoteState;
use artidezi_store::model::{MergeRequestState, PublishedMergeRequest, Task};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::{Orchestrator, OrchestratorError, Result};

impl Orchestrator {
    pub(crate) fn cleanup_proposal_branch(
        &self,
        task: &Task,
        attempt_id: Option<&str>,
        policy: &ResolvedProjectPolicy,
        mr: &PublishedMergeRequest,
        now: DateTime<Utc>,
    ) {
        let queued = if mr.state == MergeRequestState::Merged {
            match self.db.begin_branch_cleanup(mr, now) {
                Ok(Some(pending)) => Some(pending),
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(task = %task.id, %error, "could not reserve branch cleanup");
                    return;
                }
            }
        } else {
            None
        };
        let (mr, tries) = queued.as_ref().map_or((mr, 1), |(mr, tries)| (mr, *tries));
        let repo = policy.repositories.get(&mr.repository_id);
        let mut payload = json!({"repository":mr.repository_id,"forge":mr.forge,"remote":repo.map(|r| &r.remote),
            "number":mr.number,"branch":mr.source_branch,"head":mr.head_commit});
        // Preserve old closed-proposal plans byte for byte; merged plans bind their terminal state.
        if mr.state == MergeRequestState::Merged {
            payload["state"] = "merged".into();
        }
        let result = (|| -> Result<()> {
            let events = self.db.list_events(&task.id)?;
            let matches = |event: &&artidezi_store::model::Event| {
                event.payload["repository"] == mr.repository_id
                    && event.payload["number"] == mr.number
            };
            if events.iter().filter(matches).any(|event| {
                matches!(
                    event.kind.as_str(),
                    "merge_request.branch_deleted"
                        | "merge_request.branch_delete_skipped"
                        | "merge_request.branch_delete_failed"
                )
            }) {
                return Ok(());
            }
            if events.iter().filter(matches).any(|event| {
                event.kind == "merge_request.branch_delete_planned" && event.payload != payload
            }) {
                return Err(invalid("branch deletion receipt changed since planning"));
            }
            if tries > 3 {
                return Err(invalid("branch cleanup exhausted three attempts"));
            }
            let repo = repo.ok_or_else(|| invalid("cleanup repository is missing"))?;
            let skipped = self.branch_cleanup_skip_reason(task, policy, repo, mr)?;
            if let Some(reason) = skipped {
                let mut skipped = payload.clone();
                skipped["reason"] = reason.into();
                self.db.record_event(
                    Some(&task.id),
                    attempt_id,
                    "orchestrator",
                    "merge_request.branch_delete_skipped",
                    skipped,
                )?;
                return Ok(());
            }
            if !events
                .iter()
                .filter(matches)
                .any(|event| event.kind == "merge_request.branch_delete_planned")
            {
                self.db.record_event(
                    Some(&task.id),
                    attempt_id,
                    "orchestrator",
                    "merge_request.branch_delete_planned",
                    payload.clone(),
                )?;
            }
            let head = git2::Oid::from_str(&mr.head_commit)?;
            if head.is_zero() {
                return Err(invalid("branch deletion requires a recorded nonzero head"));
            }
            let forge = self.forge(&repo.forge)?;
            let credentials = forge.git_push_credentials(&repo.remote)?;
            self.cache(&repo.forge, &repo.remote)?.push_with_lease(
                &forge.clone_url(&repo.remote),
                &format!(":refs/heads/{}", mr.source_branch),
                credentials.as_ref(),
                head,
            )?;
            self.db.record_event(
                Some(&task.id),
                attempt_id,
                "orchestrator",
                "merge_request.branch_deleted",
                payload.clone(),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(task = %task.id, repository = %mr.repository_id, %error,
                "proposal branch cleanup failed; delivery continues");
            // A stale lease is a definitive refusal. Transport/API errors may clear, but even a
            // crash consumes a try. Reserve the deadline before effects, including a lost response.
            let retry = queued.is_some()
                && tries < 3
                && !matches!(error, OrchestratorError::Invalid(_))
                && !error.to_string().contains("push lease failed");
            let mut failed = payload;
            failed["reason"] = error.to_string().into();
            if retry {
                failed["attempt"] = tries.into();
                failed["next_attempt_at"] =
                    (now + chrono::Duration::seconds(60)).to_rfc3339().into();
            }
            if let Err(error) = self.db.record_event(
                Some(&task.id),
                attempt_id,
                "orchestrator",
                if retry {
                    "merge_request.branch_delete_retry_scheduled"
                } else {
                    "merge_request.branch_delete_failed"
                },
                failed,
            ) {
                tracing::warn!(task = %task.id, %error, "could not record branch cleanup failure");
                return;
            }
            if retry {
                return;
            }
        }
        // Write the terminal event first: recovery after a crash here retires the queue row
        // without touching a branch that may already have been recreated.
        if queued.is_some()
            && let Err(error) = self.db.finish_branch_cleanup(mr)
        {
            tracing::warn!(task = %task.id, %error, "could not finish branch cleanup");
        }
    }

    fn branch_cleanup_skip_reason(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        repo: &ResolvedRepository,
        mr: &PublishedMergeRequest,
    ) -> Result<Option<&'static str>> {
        if self
            .require_proposal_cleanup_authority(task, policy, &mr.repository_id, repo)
            .is_err()
        {
            return Ok(Some("publication authority no longer permits cleanup"));
        }
        if repo.access != RepositoryAccess::ReadWrite
            || mr.task_id != task.id
            || mr.forge != repo.forge
            || mr.source_branch != format!("artidezi/{}", task.id)
        {
            return Ok(Some("receipt does not grant cleanup of this branch"));
        }
        let forge = self.forge(&repo.forge)?;
        let expected_state = match mr.state {
            MergeRequestState::Closed => RemoteState::Closed,
            MergeRequestState::Merged => RemoteState::Merged,
            MergeRequestState::Open => return Ok(Some("proposal is not terminal")),
        };
        // Confirm the exact terminal state, including recovery after a lost response. Never
        // turn a fresh observed branch OID into deletion authority.
        let info = forge.get_merge_request(&repo.remote, mr.number)?;
        if info.state != expected_state
            || info.author_login != self.config.forges[&repo.forge].bot_login
            || info.source_branch != mr.source_branch
            || info.target_branch != repo.branch
            || info.head_commit.as_str() != mr.head_commit
        {
            return Ok(Some(
                "terminal proposal no longer matches the owned receipt",
            ));
        }
        if forge
            .proposals_for_branch(&repo.remote, &mr.source_branch)?
            .iter()
            .any(|proposal| proposal.state == RemoteState::Open)
        {
            return Ok(Some("an open proposal uses this branch"));
        }
        if self.branch_has_pending_publication(task, repo, &mr.source_branch)? {
            return Ok(Some("an unresolved publication uses this branch"));
        }
        Ok(None)
    }

    fn require_proposal_cleanup_authority(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        repository_id: &str,
        repo: &ResolvedRepository,
    ) -> Result<()> {
        if policy.source_pull_request.is_some() {
            return self.require_source_authority(task, policy);
        }
        let project = self
            .config
            .projects
            .get(&task.project)
            .ok_or_else(|| invalid("project removed"))?;
        let current = project
            .repositories
            .get(repository_id)
            .ok_or_else(|| invalid("repository removed"))?;
        let forge = self
            .config
            .forges
            .get(&repo.forge)
            .ok_or_else(|| invalid("forge removed"))?;
        if !policy.effects.publish_adaptation_merge_requests
            || !project.effects.publish_adaptation_merge_requests
            || current.agent_access != RepositoryAccess::ReadWrite
            || current.forge != repo.forge
            || current.remote != repo.remote
            || current.branch != repo.branch
            || forge.provider != repo.forge_provider
            || repo
                .forge_api_base_url
                .as_ref()
                .is_some_and(|url| url != &forge.api_base_url)
            || repo
                .forge_bot_login
                .as_ref()
                .is_some_and(|bot| bot != &forge.bot_login)
            || (repo.forge_provider == artidezi_config::model::ForgeProvider::Github
                && (!policy.effects.allow_manual_github_publication
                    || !project.effects.allow_manual_github_publication))
            || !self.forge(&repo.forge)?.writes_allowed_for(&repo.remote)
        {
            return Err(invalid(
                "repository identity or publication permission changed",
            ));
        }
        Ok(())
    }

    /// Branches are named after their task, so only that task's plans can use one.
    fn branch_has_pending_publication(
        &self,
        task: &Task,
        repo: &ResolvedRepository,
        branch: &str,
    ) -> Result<bool> {
        // Plans can represent successful remote writes whose receipts were lost. Include repository
        // aliases; local lifecycle state alone cannot settle those writes.
        let events = self.db.list_events(&task.id)?;
        let plans: Vec<_> = events
            .iter()
            .filter(|event| {
                (event.kind == "publication.planned" && event.payload["branch"] == branch)
                    || (event.kind == "manual_publication.planned"
                        && branch == format!("artidezi/{}", task.id))
            })
            .collect();
        if plans.is_empty() {
            return Ok(false);
        }
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|_| invalid("cannot inspect publication repository identity"))?;
        let receipts = self.db.list_merge_requests(&task.id)?;
        for plan in plans {
            let repository = plan.payload["repository"]
                .as_str()
                .ok_or_else(|| invalid("publication plan has no repository"))?;
            let other = policy
                .repositories
                .get(repository)
                .ok_or_else(|| invalid("publication repository is missing"))?;
            if other.forge_provider != repo.forge_provider
                || other.forge_api_base_url != repo.forge_api_base_url
                || !other.remote.eq_ignore_ascii_case(&repo.remote)
            {
                continue;
            }
            if !receipts.iter().any(|receipt| {
                receipt.repository_id == repository
                        && receipt.forge == other.forge
                        && receipt.source_branch == branch
                        && receipt.state != artidezi_store::model::MergeRequestState::Open
                        // An older closed receipt cannot settle a different pending candidate.
                        // Publication events also settle earlier heads of a revised proposal.
                        && (plan.payload["head"] == receipt.head_commit
                            || events.iter().any(|event| {
                                event.kind == "merge_request.published"
                                    && event.payload["repository"] == repository
                                    && event.payload["number"] == receipt.number
                                    && event.payload["head"] == plan.payload["head"]
                            }))
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn invalid(message: &str) -> OrchestratorError {
    OrchestratorError::Invalid(message.into())
}
