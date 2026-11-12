// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Best-effort cleanup in the supersession close path. A crash retries only the recorded lease;
//! a terminal cleanup outcome is never retried, even if the branch is subsequently recreated.

use artidezi_config::{ResolvedProjectPolicy, resolve::ResolvedRepository};
use artidezi_contract::common::RepositoryAccess;
use artidezi_forge::MergeRequestState as RemoteState;
use artidezi_store::model::{PublishedMergeRequest, Task};
use serde_json::json;

use crate::{Orchestrator, OrchestratorError, Result};

impl Orchestrator {
    pub(crate) fn cleanup_superseded_branch(
        &self,
        task: &Task,
        attempt_id: &str,
        policy: &ResolvedProjectPolicy,
        mr: &PublishedMergeRequest,
    ) {
        // The caller already checked the frozen receipt's repository identity before closing.
        let repo = &policy.repositories[&mr.repository_id];
        let payload = json!({"repository":mr.repository_id,"forge":mr.forge,"remote":repo.remote,
            "number":mr.number,"branch":mr.source_branch,"head":mr.head_commit});
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
            let skipped = self.branch_cleanup_skip_reason(task, policy, repo, mr)?;
            if let Some(reason) = skipped {
                let mut skipped = payload.clone();
                skipped["reason"] = reason.into();
                self.db.record_event(
                    Some(&task.id),
                    Some(attempt_id),
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
                    Some(attempt_id),
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
                Some(attempt_id),
                "orchestrator",
                "merge_request.branch_deleted",
                payload.clone(),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(task = %task.id, repository = %mr.repository_id, %error,
                "superseded branch cleanup failed; retiring task without further cleanup");
            let mut failed = payload;
            failed["reason"] = error.to_string().into();
            if let Err(error) = self.db.record_event(
                Some(&task.id),
                Some(attempt_id),
                "orchestrator",
                "merge_request.branch_delete_failed",
                failed,
            ) {
                tracing::warn!(task = %task.id, %error, "could not record branch cleanup failure");
            }
        }
    }

    fn branch_cleanup_skip_reason(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        repo: &ResolvedRepository,
        mr: &PublishedMergeRequest,
    ) -> Result<Option<&'static str>> {
        self.require_source_authority(task, policy)?;
        if repo.access != RepositoryAccess::ReadWrite
            || mr.forge != repo.forge
            || mr.source_branch != format!("artidezi/{}", task.id)
        {
            return Ok(Some("receipt does not grant cleanup of this branch"));
        }
        let forge = self.forge(&repo.forge)?;
        // Closure must still be observable at the receipt's head, including recovery after a
        // lost close response. Never turn a fresh observed branch OID into deletion authority.
        let info = forge.get_merge_request(&repo.remote, mr.number)?;
        if info.state != RemoteState::Closed
            || info.author_login != self.config.forges[&repo.forge].bot_login
            || info.source_branch != mr.source_branch
            || info.target_branch != repo.branch
            || info.head_commit.as_str() != mr.head_commit
        {
            return Ok(Some("closed proposal no longer matches the owned receipt"));
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
