// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Opt-in merging of ArtiDezi's own adaptation PRs for discovered sources.
//!
//! A handler with `adaptation_action: merge` lets the reconcile pass merge the adaptations of an
//! unchanged source once every one of them has successful checks and nobody requests changes.
//! Anything unexpected leaves the adaptations with a person: a failed or refused merge is not
//! retried automatically for that head.

use crate::{Orchestrator, Result};
use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::{AdaptationAction, PullRequestMode};
use artidezi_forge::{ChecksState, ExpectedProposal, MergeMethod};
use artidezi_store::model::*;
use chrono::Utc;

pub(crate) const MERGE_PENDING_STATUS: &str =
    "Published adaptations merge once their checks pass, unless a reviewer requests changes";

impl Orchestrator {
    /// Whether the task and the current configuration both still let its adaptations merge.
    pub(crate) fn adaptation_merge_authorized(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
    ) -> Result<bool> {
        let Some(source) = policy.source_pull_request.as_ref() else {
            return Ok(false);
        };
        let Some(project) = self.config.projects.get(&task.project) else {
            return Ok(false);
        };
        let handler = project
            .pull_requests
            .as_ref()
            .and_then(|d| d.handlers.iter().find(|h| h.id == source.handler_id));
        if source.adaptation_action != AdaptationAction::Merge
            || !policy.effects.merge_adaptation_merge_requests
            || !project.effects.merge_adaptation_merge_requests
            || policy.draft_merge_requests
            || project.draft_merge_requests
            || handler.is_none_or(|h| {
                h.mode != PullRequestMode::Publish || h.adaptation_action != AdaptationAction::Merge
            })
        {
            return Ok(false);
        }
        // Protected CI path changes needed a person to publish them; they also need one to merge.
        Ok(!self
            .db
            .list_events(&task.id)?
            .iter()
            .any(|e| e.kind == "publication.workflow_authorized"))
    }

    /// Whether the source, the configuration and every review still allow an adaptation merge.
    /// Rechecked before each merge, since an earlier one can make the updater move the source.
    fn adaptation_merge_gates_hold(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
    ) -> Result<bool> {
        // Authority, pause, tracking, source identity, routing, title, draft state and head.
        if self.require_source_fresh(task, policy, false).is_err() {
            return Ok(false);
        }
        Ok(self.adaptation_merge_authorized(task, policy)?
            && !self.source_changes_requested(policy)?)
    }

    /// Whether one adaptation can merge now: unchanged, not a draft, green and without objections.
    fn adaptations_ready(
        &self,
        policy: &ResolvedProjectPolicy,
        proposals: &[PublishedMergeRequest],
    ) -> Result<bool> {
        for mr in proposals
            .iter()
            .filter(|mr| mr.state == MergeRequestState::Open)
        {
            if !self.adaptation_ready(policy, mr)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn adaptation_ready(
        &self,
        policy: &ResolvedProjectPolicy,
        mr: &PublishedMergeRequest,
    ) -> Result<bool> {
        let repo = &policy.repositories[&mr.repository_id];
        let current = self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, mr.number)?;
        let head = artidezi_contract::common::CommitSha(mr.head_commit.clone());
        let expected = ExpectedProposal {
            number: mr.number,
            source_branch: &mr.source_branch,
            target_branch: &repo.branch,
            head: &head,
        };
        // Still this bot's open proposal from its branch, at the receipt head, into the target.
        Ok(
            expected.matches(&current, &self.config.forges[&repo.forge].bot_login)
                && !current.draft
                && current.checks == ChecksState::Success
                && !self.changes_requested(&repo.forge, &repo.remote, mr.number)?,
        )
    }

    /// Merges the task's open adaptations when all of them are ready. `proposals` were just
    /// reconciled against the forge.
    pub(crate) fn merge_ready_adaptations(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
        proposals: &mut [PublishedMergeRequest],
    ) -> Result<()> {
        if task.state != TaskState::AwaitingReview
            || proposals
                .iter()
                .any(|mr| mr.state == MergeRequestState::Closed)
            || !proposals
                .iter()
                .any(|mr| mr.state == MergeRequestState::Open)
            || !self.adaptation_merge_authorized(task, policy)?
        {
            return Ok(());
        }
        let frozen = policy.source_pull_request.as_ref().unwrap();
        // A plan for an adaptation's current head was merged (and is no longer open), refused or
        // interrupted by a crash. The latter two are left to a person rather than tried again.
        let events = self.db.list_events(&task.id)?;
        let interrupted = proposals.iter().any(|mr| {
            mr.state == MergeRequestState::Open
                && events.iter().any(|e| {
                    e.kind == "adaptation.merge_planned"
                        && e.payload["repository"] == mr.repository_id.as_str()
                        && e.payload["number"] == mr.number
                        && e.payload["head"] == mr.head_commit.as_str()
                })
        });
        if interrupted {
            let mut tracked = self.db.get_pull_request_source(&frozen.tracking_id)?;
            let status =
                "Automatic adaptation merge did not complete; published adaptations await review";
            if tracked.status != status {
                tracked.status = status.into();
                self.db.save_pull_request_source(&tracked)?;
            }
            return Ok(());
        }
        for index in 0..proposals.len() {
            if proposals[index].state != MergeRequestState::Open {
                continue;
            }
            // Before every merge, the source, configuration and every remaining adaptation must
            // still qualify: an earlier merge can lead the updater to close or rebase the source,
            // and anyone can object or retarget meanwhile.
            if !self.adaptation_merge_gates_hold(task, policy)?
                || !self.adaptations_ready(policy, proposals)?
            {
                return Ok(());
            }
            let mr = &mut proposals[index];
            let repo = policy.repositories[&mr.repository_id].clone();
            let payload = serde_json::json!({
                "repository": mr.repository_id,
                "number": mr.number,
                "head": mr.head_commit,
            });
            self.db.record_event(
                Some(&task.id),
                Some(&attempt.id),
                "orchestrator",
                "adaptation.merge_planned",
                payload.clone(),
            )?;
            let head = artidezi_contract::common::CommitSha(mr.head_commit.clone());
            match self
                .forge(&repo.forge)?
                .merge(&repo.remote, mr.number, &head, MergeMethod::Merge)
            {
                Ok(commit) => {
                    mr.state = MergeRequestState::Merged;
                    mr.updated_at = Utc::now();
                    self.db.upsert_merge_request(mr)?;
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "merge_request.automerged",
                        serde_json::json!({"repository": mr.repository_id, "number": mr.number, "commit": commit}),
                    )?;
                    self.cleanup_proposal_branch(task, Some(&attempt.id), policy, mr, Utc::now());
                }
                Err(error) => {
                    let mut payload = payload;
                    payload["error"] = error.to_string().into();
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "adaptation.merge_failed",
                        payload,
                    )?;
                    let mut tracked = self.db.get_pull_request_source(&frozen.tracking_id)?;
                    tracked.status =
                        "Automatic adaptation merge failed; published adaptations await review"
                            .into();
                    self.db.save_pull_request_source(&tracked)?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
