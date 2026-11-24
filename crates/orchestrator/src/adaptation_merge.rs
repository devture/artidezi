// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Opt-in merging of ArtiDezi's own adaptation PRs for discovered sources.
//!
//! A handler with `adaptation_action: merge` lets the reconcile pass merge the adaptations of an
//! unchanged source once every one of them has successful checks and nobody requests changes.
//! A temporary failure or an interrupted merge is tried again for the same head, after looking for
//! a merge that happened anyway. Anything else unexpected leaves the adaptations with a person: a
//! refused merge is not retried automatically for that head.

use crate::{Orchestrator, Result};
use artidezi_config::ResolvedProjectPolicy;
use artidezi_config::model::{AdaptationAction, PullRequestMode};
use artidezi_contract::time::{SignedDuration, Timestamp};
use artidezi_forge::{ChecksState, ExpectedProposal, MergeMethod};
use artidezi_store::model::*;

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
        // A plan for an adaptation that is still open at the planned head was refused, failed
        // temporarily or was interrupted. Merging is bound to that head, so only refusals are final.
        let limits = self.delivery_retries(task);
        let events = self.db.list_events(&task.id)?;
        let mut earlier_tries = Vec::new();
        for mr in proposals
            .iter()
            .filter(|mr| mr.state == MergeRequestState::Open)
        {
            let tries = adaptation_merge_tries(&events, mr);
            if tries.refused || tries.count > limits.max {
                earlier_tries.clear();
                earlier_tries.push(None);
                break;
            }
            if let Some(at) = tries.last {
                let delay = limits.delay_seconds(tries.count - 1);
                earlier_tries.push(Some(at + SignedDuration::from_secs(i64::from(delay))));
            }
        }
        if earlier_tries
            .iter()
            .flatten()
            .any(|at| *at > Timestamp::now())
        {
            return Ok(());
        }
        self.clear_delivery_hold(task)?;
        if earlier_tries.contains(&None) {
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
            self.forge(&repo.forge)?;
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
                    mr.updated_at = Timestamp::now();
                    self.db.upsert_merge_request(mr)?;
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "merge_request.automerged",
                        serde_json::json!({"repository": mr.repository_id, "number": mr.number, "commit": commit}),
                    )?;
                    self.cleanup_proposal_branch(
                        task,
                        Some(&attempt.id),
                        policy,
                        mr,
                        Timestamp::now(),
                    );
                }
                Err(error) => {
                    let mut payload = payload;
                    let temporary = error.is_temporary();
                    payload["error"] = error.to_string().into();
                    payload["temporary"] = temporary.into();
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "adaptation.merge_failed",
                        payload,
                    )?;
                    let mut tracked = self.db.get_pull_request_source(&frozen.tracking_id)?;
                    let tries = adaptation_merge_tries(&self.db.list_events(&task.id)?, mr);
                    if temporary && tries.count <= limits.max {
                        let delay = limits.delay_seconds(tries.count - 1);
                        let mut hold = self.new_hold(
                            task,
                            HoldReason::Delivery,
                            format!("{error} while merging the adaptation"),
                        )?;
                        hold.releases = vec![HoldRelease::Time];
                        hold.retries = tries.count;
                        hold.max_retries = limits.max;
                        hold.next_at =
                            Some(Timestamp::now() + SignedDuration::from_secs(i64::from(delay)));
                        tracked.place_hold(hold);
                        self.db.save_pull_request_source(&tracked)?;
                        return Ok(());
                    }
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

struct MergeTries {
    count: u32,
    last: Option<Timestamp>,
    refused: bool,
}

/// Earlier automatic merges of this adaptation at its recorded head.
fn adaptation_merge_tries(events: &[Event], mr: &PublishedMergeRequest) -> MergeTries {
    let of_this_head = |event: &&Event| {
        event.payload["repository"] == mr.repository_id.as_str()
            && event.payload["number"] == mr.number
            && event.payload["head"] == mr.head_commit.as_str()
    };
    let planned = || {
        events
            .iter()
            .filter(of_this_head)
            .filter(|event| event.kind == "adaptation.merge_planned")
    };
    MergeTries {
        count: planned().count() as u32,
        last: planned().map(|event| event.at).max(),
        refused: events.iter().filter(of_this_head).any(|event| {
            event.kind == "adaptation.merge_failed" && event.payload["temporary"] != true
        }),
    }
}
