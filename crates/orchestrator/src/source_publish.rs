// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Opt-in publication, approval and merge of source assessments. No automatic releases.
//! Plans precede remote effects in the durable event log; explicit retries reconcile receipts.

use crate::{Orchestrator, OrchestratorError, Result, discovery::source_of};
use artidezi_config::model::{DeliveryMode, PullRequestAction, PullRequestMode, SafeVerdictAction};
use artidezi_config::{ResolvedProjectPolicy, ResolvedRepository};
use artidezi_contract::{
    Job,
    attempt::{AttemptRecord, RepositoryCollection},
    common::{CommitSha, Outcome, RepositoryAccess},
    result::AgentResult,
};
use artidezi_forge::{MergeRequestState as RemoteState, NewMergeRequest};
use artidezi_store::model::*;
use chrono::Utc;
use git2::Oid;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt::Write as _};

#[derive(Serialize, Deserialize)]
struct ProposalPlan {
    repository: String,
    branch: String,
    head: CommitSha,
    cache_ref: String,
    title: String,
    body: String,
}

fn invalid(message: &str) -> OrchestratorError {
    OrchestratorError::Invalid(message.into())
}
pub(crate) fn limited(text: &str, limit: usize) -> String {
    let mut value: String = text.trim().chars().take(limit).collect();
    if text.trim().chars().count() > limit {
        value.push('…');
    }
    // Agent prose must not send unsolicited mentions to repository participants.
    value.replace('@', "@\u{200b}")
}
fn summary(result: &AgentResult) -> String {
    let mut body = limited(&result.summary, 3000);
    if let Some(risk) = &result.risk_hint {
        body.push_str(&format!(
            "\n\n**Agent-assessed risk: {:?}.** {}",
            risk.level,
            limited(&risk.rationale, 600)
        ));
    }
    if !result.questions_for_human.is_empty() {
        let heading = if result.questions_for_human.len() == 1 {
            "Maintainer question"
        } else {
            "Maintainer questions"
        };
        body.push_str(&format!("\n\n### {heading}\n"));
        for question in &result.questions_for_human {
            body.push_str(&format!("\n- {}", limited(question, 600)));
        }
    }
    body
}

fn evidence_kind(kind: artidezi_contract::result::EvidenceKind) -> &'static str {
    match kind {
        artidezi_contract::result::EvidenceKind::ReleaseNotes => "Release notes",
        artidezi_contract::result::EvidenceKind::Changelog => "Changelog",
        artidezi_contract::result::EvidenceKind::UpstreamDiff => "Upstream diff",
        artidezi_contract::result::EvidenceKind::Documentation => "Documentation",
        artidezi_contract::result::EvidenceKind::LocalCheck => "Local check",
        artidezi_contract::result::EvidenceKind::Other => "Evidence",
    }
}

pub(crate) fn plain_note(value: &str) -> String {
    limited(&value.replace(['\n', '\r'], " "), 600)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn markdown_label(value: &str, limit: usize) -> String {
    limited(value, limit)
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(crate) fn inline_code(value: &str, limit: usize) -> String {
    let value = limited(value, limit).replace(['\n', '\r'], " ");
    let longest = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    format!("{fence} {value} {fence}")
}

fn safe_url(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '(' => "%28".to_owned(),
            ')' => "%29".to_owned(),
            '\n' | '\r' => " ".to_owned(),
            _ => character.to_string(),
        })
        .collect()
}

fn evidence_source(source: &str, repository: &ResolvedRepository) -> String {
    let source = source.trim();
    if !(source.starts_with("https://") || source.starts_with("http://")) {
        return inline_code(source, 500);
    }

    let Some((prefix, path)) = source
        .split_once("//")
        .and_then(|(_, rest)| rest.split_once('/'))
    else {
        return inline_code(source, 500);
    };
    let segments: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let commit_index = segments.iter().position(|part| *part == "commit");
    let is_same_repository = segments
        .windows(2)
        .any(|window| format!("{}/{}", window[0], window[1]) == repository.remote);
    let Some(commit_index) = commit_index.filter(|index| segments.get(index + 1).is_some()) else {
        return format!("[source]({})", safe_url(source));
    };
    let commit = segments[commit_index + 1];
    let commit_label: String = commit.chars().take(12).collect();
    if is_same_repository {
        let repo_url = format!("https://{prefix}/{}", repository.remote);
        return format!(
            "[{}]({repo_url}) @ [{}]({})",
            markdown_label(&repository.remote, 300),
            markdown_label(&commit_label, 32),
            safe_url(source)
        );
    }
    let Some(repo_name) = segments.get(0..2).map(|parts| parts.join("/")) else {
        return format!("[source]({})", safe_url(source));
    };
    let repo_url = format!("https://{prefix}/{repo_name}");
    format!(
        "[{}]({repo_url}) @ [{}]({})",
        markdown_label(&repo_name, 300),
        markdown_label(&commit_label, 32),
        safe_url(source)
    )
}

fn evidence_section(result: &AgentResult, repository: &ResolvedRepository) -> String {
    if result.evidence.is_empty() {
        return String::new();
    }
    let count = result.evidence.len();
    let noun = if count == 1 { "item" } else { "items" };
    let mut body = format!("<details>\n<summary>Evidence ({count} {noun})</summary>\n\n");
    for evidence in &result.evidence {
        let source = evidence_source(&evidence.source, repository);
        let note = plain_note(&evidence.note);
        if note.is_empty() {
            writeln!(body, "- **{}:** {}", evidence_kind(evidence.kind), source).unwrap();
        } else {
            writeln!(
                body,
                "- **{}:** {} — {}",
                evidence_kind(evidence.kind),
                source,
                note
            )
            .unwrap();
        }
    }
    body.push_str("\n</details>\n");
    body
}

impl Orchestrator {
    pub(crate) fn protected_repositories(
        &self,
        policy: &ResolvedProjectPolicy,
        record: &AttemptRecord,
        result: &AgentResult,
    ) -> Result<BTreeSet<String>> {
        let mut repositories = BTreeSet::new();
        for collection in &record.repositories {
            if !result
                .repositories
                .iter()
                .any(|r| r.id == collection.id && r.changed)
            {
                continue;
            }
            let resolved = &policy.repositories[collection.id.as_str()];
            if self.collection_changes_protected_paths(resolved, collection)? {
                repositories.insert(collection.id.to_string());
            }
        }
        Ok(repositories)
    }

    fn collection_changes_protected_paths(
        &self,
        resolved: &ResolvedRepository,
        collection: &RepositoryCollection,
    ) -> Result<bool> {
        let series = collection
            .series
            .as_ref()
            .ok_or_else(|| invalid("missing series"))?;
        let protected = self.forge(&resolved.forge)?.protected_paths();
        let cache = self.cache(&resolved.forge, &resolved.remote)?;
        let base = cache
            .repo()
            .find_commit(Oid::from_str(collection.base_commit.as_str())?)?;
        for commit in &series.commits {
            let head = cache
                .repo()
                .find_commit(Oid::from_str(commit.sha.as_str())?)?;
            let diff =
                cache
                    .repo()
                    .diff_tree_to_tree(Some(&base.tree()?), Some(&head.tree()?), None)?;
            if diff.deltas().any(|delta| {
                [delta.old_file().path(), delta.new_file().path()]
                    .iter()
                    .flatten()
                    .any(|path| protected.iter().any(|prefix| path.starts_with(prefix)))
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn workflow_publication_authorized(
        &self,
        task: &Task,
        attempt: &Attempt,
    ) -> Result<bool> {
        let record = attempt
            .attempt_record
            .as_ref()
            .ok_or_else(|| invalid("missing attempt record"))?;
        Ok(self.db.list_events(&task.id)?.iter().any(|event| {
            event.kind == "publication.workflow_authorized"
                && event.attempt_id.as_deref() == Some(&attempt.id)
                && event.payload["attempt_record_sha256"] == record.sha256
        }))
    }

    pub(crate) fn require_source_authority(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
    ) -> Result<()> {
        let source = policy
            .source_pull_request
            .as_ref()
            .ok_or_else(|| invalid("missing source identity"))?;
        if task.kind != TaskKind::PullRequest
            || task.delivery_mode != DeliveryMode::MergeRequest
            || policy.delivery_mode != task.delivery_mode
            || !policy.effects.publish_adaptation_merge_requests
            || source.publisher_login.is_none()
        {
            return Err(invalid(
                "task was not authorized for assessment publication",
            ));
        }
        let project = self
            .config
            .projects
            .get(&task.project)
            .ok_or_else(|| invalid("project removed"))?;
        let discovery = project
            .pull_requests
            .as_ref()
            .ok_or_else(|| invalid("discovery disabled"))?;
        let handler = discovery.handlers.iter().find(|h| {
            source
                .title
                .as_ref()
                .is_some_and(|title| h.matches_pull_request(&source.author, title))
                || (source.title.is_none()
                    && h.title_regex.is_none()
                    && h.matches_pull_request(&source.author, ""))
        });
        if !project.effects.publish_adaptation_merge_requests
            || !discovery.repositories.contains(&source.repository_id)
            || handler.is_none_or(|h| {
                h.id != source.handler_id
                    || h.mode != PullRequestMode::Publish
                    || h.action != PullRequestAction::Assess
                    || h.safe_verdict_action != source.safe_verdict_action
            })
        {
            return Err(invalid(
                "current configuration no longer permits publication",
            ));
        }
        if source.safe_verdict_action != SafeVerdictAction::Comment {
            let handler = handler.unwrap();
            let repo = &policy.repositories[&source.repository_id];
            let custom_renovate_author = source.author.to_ascii_lowercase();
            let is_renovate = custom_renovate_author == "renovate[bot]"
                || custom_renovate_author.ends_with("-renovate[bot]")
                || (custom_renovate_author.starts_with("app/")
                    && custom_renovate_author.ends_with("renovate"));
            if handler.catch_all
                || repo.forge_provider != artidezi_config::model::ForgeProvider::Github
                || repo.access != RepositoryAccess::ReadWrite
                || (source.safe_verdict_action == SafeVerdictAction::Merge
                    && (!policy.effects.merge_renovate_merge_request_on_safe_verdict
                        || !project.effects.merge_renovate_merge_request_on_safe_verdict
                        || !is_renovate))
            {
                return Err(invalid(
                    "source approval/merge authority is absent or changed",
                ));
            }
        }
        for (id, repo) in &policy.repositories {
            let current = project
                .repositories
                .get(id)
                .ok_or_else(|| invalid("repository removed"))?;
            let forge = self
                .config
                .forges
                .get(&repo.forge)
                .ok_or_else(|| invalid("forge removed"))?;
            if current.forge != repo.forge
                || current.remote != repo.remote
                || current.branch != repo.branch
                || forge.provider != repo.forge_provider
                || repo.forge_api_base_url.as_ref() != Some(&forge.api_base_url)
                || repo.forge_bot_login.as_ref() != Some(&forge.bot_login)
                || current.allowed_paths != repo.allowed_paths
                || (repo.access == RepositoryAccess::ReadWrite
                    && (current.agent_access != RepositoryAccess::ReadWrite
                        || !self.forge(&repo.forge)?.writes_allowed_for(&repo.remote)))
            {
                return Err(invalid(
                    "repository identity or publication permission changed",
                ));
            }
        }
        let repository = &policy.repositories[&source.repository_id];
        if Some(&self.config.forges[&repository.forge].bot_login) != source.publisher_login.as_ref()
        {
            return Err(invalid(
                "publishing bot changed; a new evaluation is required",
            ));
        }
        Ok(())
    }

    fn require_source_fresh(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
        intake_checks: bool,
    ) -> Result<()> {
        self.require_source_authority(task, policy)?;
        let source = policy.source_pull_request.as_ref().unwrap();
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if tracked.paused || tracked.last_task_id.as_deref() != Some(&task.id) {
            return Err(invalid("source is paused or a newer evaluation exists"));
        }
        let repo = &policy.repositories[&source.repository_id];
        let current = self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, source.number)?;
        let handler = self.config.projects[&task.project]
            .pull_requests
            .as_ref()
            .unwrap()
            .handlers
            .iter()
            .find(|h| h.id == source.handler_id)
            .unwrap();
        if current.state != RemoteState::Open
            || current.source_branch != source.source_branch
            || current.target_branch != source.target_branch
            || current.author_login != source.author
            || source
                .title
                .as_ref()
                .is_some_and(|title| current.title != *title)
            || current.draft
        {
            return Err(invalid(
                "source identity, routing, title, state or draft status changed; evaluate again",
            ));
        }
        if current.head_commit != source.input.commit {
            return Err(OrchestratorError::SourceHeadChanged {
                expected: source.input.commit.clone(),
                actual: current.head_commit,
            });
        }
        if intake_checks
            && handler.require_successful_checks
            && current.checks != artidezi_forge::ChecksState::Success
        {
            return Err(invalid("source checks changed; evaluate again"));
        }
        // Target branches may advance while the agent works. Adaptation publication replays the
        // validated commit series onto the current target and refuses conflicts; untouched
        // approvals are merged by the forge against its current target.
        Ok(())
    }

    pub(crate) fn publish_source_result(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        job: &Job,
        record: &AttemptRecord,
        result: &AgentResult,
    ) -> Result<()> {
        match self.publish_source_result_inner(task, attempt, job, record, result) {
            Err(OrchestratorError::WaitingForCi) => self.wait_for_source_ci(task),
            Err(OrchestratorError::ChecksFailed) => self.hold_for_failed_checks(task, attempt),
            Err(error) => {
                if self.reschedule_superseded_source(task, attempt, &error)? {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            other => other,
        }
    }

    fn publish_source_result_inner(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        _job: &Job,
        record: &AttemptRecord,
        result: &AgentResult,
    ) -> Result<()> {
        let policy: ResolvedProjectPolicy =
            serde_json::from_str(&task.policy_snapshot).map_err(|e| invalid(&e.to_string()))?;
        self.require_source_authority(task, &policy)?;
        let automatic = result.outcome == Outcome::ApproveUntouched
            && policy
                .source_pull_request
                .as_ref()
                .unwrap()
                .safe_verdict_action
                != SafeVerdictAction::Comment;
        if automatic && self.recover_completed_source_action(task, attempt, &policy)? {
            return Ok(());
        }
        self.require_source_fresh(task, &policy, !automatic)?;
        if result.outcome == Outcome::Failed {
            return self.needs_human(task, attempt, "agent reported failure; nothing published");
        }
        if result.outcome == Outcome::Proposal {
            let workflow_repositories = self.protected_repositories(&policy, record, result)?;
            if !workflow_repositories.is_empty()
                && !self.workflow_publication_authorized(task, attempt)?
            {
                return Err(invalid(
                    "Protected CI path changes require task-specific human authorization; publication withheld",
                ));
            }
            for collection in &record.repositories {
                if !result
                    .repositories
                    .iter()
                    .any(|r| r.id == collection.id && r.changed)
                {
                    continue;
                }
                let resolved = &policy.repositories[collection.id.as_str()];
                let previous = self.db.list_events(&task.id)?.into_iter().find(|e| {
                    e.kind == "publication.planned"
                        && e.attempt_id.as_deref() == Some(&attempt.id)
                        && e.payload["repository"] == collection.id.as_str()
                });
                let plan: ProposalPlan = if let Some(event) = previous {
                    serde_json::from_value(event.payload).map_err(|e| invalid(&e.to_string()))?
                } else {
                    let (head, cache_ref) =
                        self.recreate_in_cache_rebased(resolved, collection, &attempt.id)?;
                    let source = policy.source_pull_request.as_ref().unwrap();
                    let title = collection
                        .series
                        .as_ref()
                        .and_then(|s| s.commits.first())
                        .and_then(|c| c.message.lines().next())
                        .unwrap_or("Adapt dependency update");
                    let plan = ProposalPlan {
                        repository: collection.id.to_string(),
                        branch: format!("artidezi/{}", task.id),
                        head,
                        cache_ref,
                        title: limited(title, 180),
                        body: format!(
                            "Adapts {} at `{}`.\n\n{}\n\n{}\nHuman review and merge are required.\n\n<!-- artidezi:proposal:{}:{} -->",
                            source.url,
                            source.input.commit,
                            summary(result),
                            evidence_section(result, resolved),
                            task.id,
                            collection.id
                        ),
                    };
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "publication.planned",
                        serde_json::to_value(&plan).map_err(|e| invalid(&e.to_string()))?,
                    )?;
                    plan
                };
                self.publish_source_candidate(
                    task,
                    attempt,
                    &policy,
                    resolved,
                    &plan,
                    workflow_repositories.contains(collection.id.as_str()),
                )?;
            }
        }
        self.require_source_fresh(task, &policy, !automatic)?;
        let source = policy.source_pull_request.as_ref().unwrap();
        let repo = &policy.repositories[&source.repository_id];
        let marker = format!("<!-- artidezi:assessment:{} -->", task.id);
        let action = source.safe_verdict_action;
        if automatic {
            self.require_source_action_ready(task, &policy)?;
        }
        let expected_state = if automatic {
            artidezi_forge::ReviewState::Approved
        } else {
            artidezi_forge::ReviewState::Commented
        };
        let verdict = match result.outcome {
            Outcome::ApproveUntouched if automatic => {
                "Approved: no adaptation is required under the configured project policy."
            }
            Outcome::ApproveUntouched => {
                "Assessment only: no adaptation is required; no approval or merge was requested."
            }
            Outcome::Proposal => {
                "The agent prepared adaptations for human review. The original change remains open."
            }
            Outcome::NeedsHuman => "The assessment needs a maintainer decision.",
            Outcome::Failed => unreachable!(),
        };
        let mut body = format!("{verdict}\n\n{}", summary(result));
        body.push_str("\n\n");
        body.push_str(&evidence_section(result, repo));
        for mr in self.db.list_merge_requests(&task.id)? {
            body.push_str(&format!("\n\nAdaptation: {}", mr.url));
        }
        body.push_str(&format!(
            "\n\nAssessed source `{}`.\n\n{marker}",
            source.input.commit
        ));
        let forge = self.forge(&repo.forge)?;
        let matching: Vec<_> = forge
            .list_reviews(&repo.remote, source.number)?
            .into_iter()
            .filter(|r| {
                Some(&r.author_login) == source.publisher_login.as_ref() && r.body.contains(&marker)
            })
            .collect();
        let review = match matching.as_slice() {
            [] => {
                self.require_source_fresh(task, &policy, !automatic)?;
                if automatic {
                    if self
                        .db
                        .list_events(&task.id)?
                        .iter()
                        .any(|e| e.kind == "assessment.approval_planned")
                    {
                        return Err(invalid(
                            "A prior approval was planned but its matching receipt is missing; inspect the source before a new evaluation",
                        ));
                    }
                    self.require_source_action_ready(task, &policy)?;
                    self.db.record_event(
                        Some(&task.id),
                        Some(&attempt.id),
                        "orchestrator",
                        "assessment.approval_planned",
                        serde_json::json!({"head":source.input.commit,"body":body}),
                    )?;
                    self.forge(&repo.forge)?.create_approval_review(
                        &repo.remote,
                        source.number,
                        &source.input.commit,
                        &body,
                    )?
                } else {
                    self.forge(&repo.forge)?.create_comment_review(
                        &repo.remote,
                        source.number,
                        &source.input.commit,
                        &body,
                    )?
                }
            }
            [review]
                if review.head_commit == source.input.commit
                    && review.body == body
                    && review.state == expected_state =>
            {
                review.clone()
            }
            _ => {
                return Err(invalid(
                    "existing assessment review differs; refusing a duplicate",
                ));
            }
        };
        self.db.record_event(Some(&task.id), Some(&attempt.id), "orchestrator", "assessment.published", serde_json::json!({"review_id":review.id,"head":source.input.commit,"outcome":result.outcome,"state":review.state}))?;
        if automatic {
            // An approval can let another actor (e.g. Renovate) merge immediately.
            if self.recover_completed_source_action(task, attempt, &policy)? {
                return Ok(());
            }
            self.require_source_action_ready(task, &policy)?;
            if action == SafeVerdictAction::Merge {
                self.db.record_event(
                    Some(&task.id),
                    Some(&attempt.id),
                    "orchestrator",
                    "source.merge_planned",
                    serde_json::json!({"head":source.input.commit,"method":"merge"}),
                )?;
                let commit = self.forge(&repo.forge)?.merge(
                    &repo.remote,
                    source.number,
                    &source.input.commit,
                    artidezi_forge::MergeMethod::Merge,
                )?;
                self.db.record_event(
                    Some(&task.id),
                    Some(&attempt.id),
                    "orchestrator",
                    "source.merge_accepted",
                    serde_json::json!({"head":source.input.commit,"commit":commit}),
                )?;
                if self.recover_completed_source_action(task, attempt, &policy)? {
                    return Ok(());
                }
                return Err(invalid(
                    "GitHub accepted the merge but its final state is not yet confirmed; retry publication",
                ));
            }
        }
        self.require_source_fresh(task, &policy, !automatic)?;
        if result.outcome == Outcome::NeedsHuman {
            return self.needs_human(
                task,
                attempt,
                "assessment published; maintainer input required",
            );
        }
        let state = if result.outcome == Outcome::Proposal {
            TaskState::AwaitingReview
        } else {
            TaskState::Done
        };
        self.db.set_task_state(&task.id, state, "orchestrator")?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        tracked.status = if state == TaskState::AwaitingReview {
            "Published adaptations await review"
        } else if automatic {
            "Source approved; merge remains subject to repository rules"
        } else {
            "Assessment review published"
        }
        .into();
        self.db.save_pull_request_source(&tracked)?;
        Ok(())
    }

    /// Whether any reviewer's latest decision requests changes. Unknown review states fail closed.
    pub(crate) fn source_changes_requested(&self, policy: &ResolvedProjectPolicy) -> Result<bool> {
        let source = policy.source_pull_request.as_ref().unwrap();
        let repo = &policy.repositories[&source.repository_id];
        let forge = self.forge(&repo.forge)?;
        let mut reviews = forge.list_reviews(&repo.remote, source.number)?;
        reviews.sort_by_key(|r| r.id);
        let mut decisions = std::collections::BTreeMap::new();
        for review in reviews {
            use artidezi_forge::ReviewState;
            match review.state {
                ReviewState::Approved | ReviewState::ChangesRequested => {
                    decisions.insert(review.author_login.to_lowercase(), review.state);
                }
                ReviewState::Other => {
                    return Err(invalid("Unknown review state; maintainer input required"));
                }
                ReviewState::Commented | ReviewState::Dismissed => {}
            }
        }
        Ok(decisions
            .values()
            .any(|s| *s == artidezi_forge::ReviewState::ChangesRequested))
    }

    /// Agent judgment is only one gate. Fresh forge checks and review objections are independent.
    pub(crate) fn require_source_action_ready(
        &self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
    ) -> Result<()> {
        self.require_source_fresh(task, policy, false)?;
        if self.source_changes_requested(policy)? {
            return Err(invalid(
                "A reviewer has requested changes; maintainer input required",
            ));
        }
        let source = policy.source_pull_request.as_ref().unwrap();
        let repo = &policy.repositories[&source.repository_id];
        match self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, source.number)?
            .checks
        {
            artidezi_forge::ChecksState::Success => Ok(()),
            artidezi_forge::ChecksState::Absent | artidezi_forge::ChecksState::Inconclusive => {
                Err(invalid(
                    "No successful forge checks; human review is required for automatic source action",
                ))
            }
            artidezi_forge::ChecksState::Pending => Err(OrchestratorError::WaitingForCi),
            artidezi_forge::ChecksState::Failure => Err(OrchestratorError::ChecksFailed),
        }
    }

    /// Recovery is read-only: observe a merged source after our exact approval was accepted.
    /// A lost merge response cannot prove who merged; do not invent attribution.
    pub(crate) fn recover_completed_source_action(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
    ) -> Result<bool> {
        self.require_source_authority(task, policy)?;
        let source = policy.source_pull_request.as_ref().unwrap();
        let tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if tracked.last_task_id.as_deref() != Some(&task.id) || tracked.paused {
            return Ok(false);
        }
        let repo = &policy.repositories[&source.repository_id];
        let forge = self.forge(&repo.forge)?;
        let current = forge.get_merge_request(&repo.remote, source.number)?;
        if current.state != RemoteState::Merged
            || current.head_commit != source.input.commit
            || current.source_branch != source.source_branch
            || current.target_branch != source.target_branch
            || current.author_login != source.author
        {
            return Ok(false);
        }
        let plan = self.db.list_events(&task.id)?.into_iter().find(|e| {
            e.kind == "assessment.approval_planned"
                && e.payload["head"] == serde_json::json!(source.input.commit)
        });
        let Some(plan) = plan else {
            return Ok(false);
        };
        let matches: Vec<_> = forge
            .list_reviews(&repo.remote, source.number)?
            .into_iter()
            .filter(|r| {
                Some(&r.author_login) == source.publisher_login.as_ref()
                    && r.head_commit == source.input.commit
                    && r.body == plan.payload["body"].as_str().unwrap_or("")
                    && r.state == artidezi_forge::ReviewState::Approved
            })
            .collect();
        if matches.len() != 1 {
            return Ok(false);
        }
        self.db.record_event(Some(&task.id), Some(&attempt.id), "forge", "source.merge_observed",
            serde_json::json!({"head":source.input.commit,"review_id":matches[0].id,"url":source.url}))?;
        self.db.set_task_state(&task.id, TaskState::Done, "forge")?;
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        tracked.closed = true;
        tracked.status = "Source merged after approval".into();
        self.db.save_pull_request_source(&tracked)?;
        Ok(true)
    }

    fn publish_source_candidate(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        policy: &ResolvedProjectPolicy,
        repo: &ResolvedRepository,
        plan: &ProposalPlan,
        workflow_changes: bool,
    ) -> Result<()> {
        self.require_source_fresh(task, policy, true)?;
        let forge = self.forge(&repo.forge)?;
        // A revision replaces this task's own open proposal.
        let owned = self
            .db
            .get_merge_request(&task.id, &plan.repository)?
            .filter(|mr| mr.state == MergeRequestState::Open && mr.source_branch == plan.branch);
        match forge.branch_head_optional(&repo.remote, &plan.branch)? {
            None => {
                let credentials = if workflow_changes {
                    forge.git_push_workflow_credentials(&repo.remote)?
                } else {
                    forge.git_push_credentials(&repo.remote)?
                };
                self.require_source_fresh(task, policy, true)?;
                self.cache(&repo.forge, &repo.remote)?.push_with_lease(
                    &forge.clone_url(&repo.remote),
                    &format!("{}:refs/heads/{}", plan.cache_ref, plan.branch),
                    credentials.as_ref(),
                    Oid::ZERO_SHA1,
                )?;
            }
            Some(head) if head == plan.head => {}
            Some(head) if owned.as_ref().is_some_and(|mr| mr.head_commit == head.0) => {
                let credentials = if workflow_changes {
                    forge.git_push_workflow_credentials(&repo.remote)?
                } else {
                    forge.git_push_credentials(&repo.remote)?
                };
                self.require_source_fresh(task, policy, true)?;
                self.cache(&repo.forge, &repo.remote)?.push_with_lease(
                    &forge.clone_url(&repo.remote),
                    &format!("+{}:refs/heads/{}", plan.cache_ref, plan.branch),
                    credentials.as_ref(),
                    Oid::from_str(head.as_str())?,
                )?;
            }
            Some(_) => return Err(invalid("proposal branch changed; refusing to overwrite it")),
        }
        self.require_source_fresh(task, policy, true)?;
        let matches = forge.proposals_for_branch(&repo.remote, &plan.branch)?;
        let info = match matches.as_slice() {
            [] => {
                self.require_source_fresh(task, policy, true)?;
                forge.create_merge_request(
                    &repo.remote,
                    &NewMergeRequest {
                        title: plan.title.clone(),
                        body: plan.body.clone(),
                        source_branch: plan.branch.clone(),
                        target_branch: repo.branch.clone(),
                        draft: policy.draft_merge_requests,
                    },
                )?
            }
            [info] if owned.is_some() && (info.title != plan.title || info.body != plan.body) => {
                forge.update_merge_request(&repo.remote, info.number, &plan.title, &plan.body)?;
                forge.get_merge_request(&repo.remote, info.number)?
            }
            [info] => info.clone(),
            _ => return Err(invalid("multiple proposals use the intended branch")),
        };
        if info.author_login != self.config.forges[&repo.forge].bot_login
            || info.source_branch != plan.branch
            || info.target_branch != repo.branch
            || info.head_commit != plan.head
            || info.body != plan.body
            || info.title != plan.title
            || info.state != RemoteState::Open
            || info.draft != policy.draft_merge_requests
        {
            return Err(invalid(
                "proposal receipt differs from the durable publication plan",
            ));
        }
        self.require_source_fresh(task, policy, true)?;
        self.db.upsert_merge_request(&PublishedMergeRequest {
            task_id: task.id.clone(),
            repository_id: plan.repository.clone(),
            forge: repo.forge.clone(),
            number: info.number,
            url: info.url.clone(),
            source_branch: plan.branch.clone(),
            head_commit: plan.head.0.clone(),
            state: MergeRequestState::Open,
            updated_at: Utc::now(),
        })?;
        self.db.record_event(Some(&task.id), Some(&attempt.id), "orchestrator", "merge_request.published", serde_json::json!({"repository":plan.repository,"number":info.number,"url":info.url,"head":plan.head}))?;
        Ok(())
    }

    pub(crate) fn require_resolved_source_proposals(&self, task: &Task) -> Result<()> {
        let policy: ResolvedProjectPolicy =
            serde_json::from_str(&task.policy_snapshot).map_err(|e| invalid(&e.to_string()))?;
        // Plans cover successful remote POSTs whose responses were lost before recording a receipt.
        for plan in self
            .db
            .list_events(&task.id)?
            .into_iter()
            .filter(|e| e.kind == "publication.planned")
        {
            let repo_id = plan.payload["repository"]
                .as_str()
                .ok_or_else(|| invalid("invalid publication plan"))?;
            let branch = plan.payload["branch"]
                .as_str()
                .ok_or_else(|| invalid("invalid publication branch"))?;
            let repo = &policy.repositories[repo_id];
            if self
                .forge(&repo.forge)?
                .proposals_for_branch(&repo.remote, branch)?
                .iter()
                .any(|mr| mr.state == RemoteState::Open)
            {
                return Err(invalid(
                    "Close or merge the existing adaptation proposals before starting another evaluation; retry publication to recover interrupted delivery",
                ));
            }
        }
        Ok(())
    }

    /// Observe human delivery without acquiring merge/tag authority or accepting a moved proposal.
    pub(crate) fn reconcile_source_task(
        &mut self,
        task: &Task,
        policy: &ResolvedProjectPolicy,
    ) -> Result<TaskState> {
        let Some(attempt) = self.db.latest_attempt(&task.id)? else {
            return Ok(task.state);
        };
        if let Err(error) = self.require_source_authority(task, policy) {
            self.needs_human(task, &attempt, &error.to_string())?;
            return Ok(TaskState::NeedsHuman);
        }
        let source = policy.source_pull_request.as_ref().unwrap();
        let repo = &policy.repositories[&source.repository_id];
        let current = self
            .forge(&repo.forge)?
            .get_merge_request(&repo.remote, source.number)?;
        let mut proposals = self.db.list_merge_requests(&task.id)?;
        for mr in &mut proposals {
            let repo = &policy.repositories[&mr.repository_id];
            let current = self
                .forge(&repo.forge)?
                .get_merge_request(&repo.remote, mr.number)?;
            if current.head_commit.as_str() != mr.head_commit
                || current.author_login != self.config.forges[&repo.forge].bot_login
                || current.source_branch != mr.source_branch
                || current.target_branch != repo.branch
            {
                self.needs_human(
                    task,
                    &attempt,
                    "Published proposal changed; maintainer review required",
                )?;
                return Ok(TaskState::NeedsHuman);
            }
            mr.state = match current.state {
                RemoteState::Open => MergeRequestState::Open,
                RemoteState::Closed => MergeRequestState::Closed,
                RemoteState::Merged => MergeRequestState::Merged,
            };
            mr.updated_at = Utc::now();
            self.db.upsert_merge_request(mr)?;
        }
        if !proposals.is_empty()
            && proposals
                .iter()
                .all(|mr| mr.state == MergeRequestState::Merged)
        {
            self.db.set_task_state(&task.id, TaskState::Done, "forge")?;
            return Ok(TaskState::Done);
        }
        if current.state == RemoteState::Open
            && current.head_commit != source.input.commit
            && self.retire_superseded_adaptations(task)?
        {
            return Ok(TaskState::Closed);
        }
        if current.head_commit != source.input.commit
            || current.state != RemoteState::Open
            || proposals
                .iter()
                .any(|mr| mr.state == MergeRequestState::Closed)
        {
            self.needs_human(task, &attempt, "Source or proposal closed or changed; resolve published proposals before evaluating again")?;
            return Ok(TaskState::NeedsHuman);
        }
        Ok(TaskState::AwaitingReview)
    }

    /// Bind one human workflow-file grant to the latest validated source attempt, then retry it.
    pub fn authorize_workflow_publication(
        &mut self,
        task_id: &str,
        expected_attempt_id: &str,
        expected_record_sha256: &str,
        reason: &str,
        actor: &str,
    ) -> Result<Task> {
        if reason.trim().is_empty()
            || reason.chars().count() > 500
            || actor.trim().is_empty()
            || actor.chars().count() > 120
        {
            return Err(invalid(
                "authorization requires a named actor and a brief reason",
            ));
        }
        let task = self.db.get_task(task_id)?;
        let held_manual =
            task.kind == TaskKind::Manual && task.delivery_mode == DeliveryMode::MergeRequest;
        if task.state != TaskState::NeedsHuman || (source_of(&task)?.is_none() && !held_manual) {
            return Err(invalid(
                "only a held source assessment or manual merge request can authorize publication",
            ));
        }
        let attempt = self
            .db
            .latest_attempt(task_id)?
            .ok_or_else(|| invalid("missing attempt"))?;
        let artifact = attempt
            .attempt_record
            .as_ref()
            .ok_or_else(|| invalid("missing attempt record"))?;
        if attempt.id != expected_attempt_id
            || attempt.state != AttemptState::Valid
            || artifact.sha256 != expected_record_sha256
        {
            return Err(invalid(
                "authorization does not match the latest validated attempt record",
            ));
        }
        let policy: ResolvedProjectPolicy =
            serde_json::from_str(&task.policy_snapshot).map_err(|e| invalid(&e.to_string()))?;
        if policy.source_pull_request.is_some() {
            self.require_source_authority(&task, &policy)?;
        }
        let record: AttemptRecord = self.artifacts.get_json(artifact)?;
        let result: AgentResult = self.artifacts.get_json(
            attempt
                .result
                .as_ref()
                .ok_or_else(|| invalid("missing result"))?,
        )?;
        if result.outcome != Outcome::Proposal {
            return Err(invalid(
                "workflow publication authorization requires a proposal",
            ));
        }
        let repositories = self.protected_repositories(&policy, &record, &result)?;
        if repositories.is_empty() {
            return Err(invalid(
                "the validated proposal has no protected CI path changes",
            ));
        }
        if self.workflow_publication_authorized(&task, &attempt)? {
            return Err(invalid(
                "this attempt is already authorized; use ordinary publication retry",
            ));
        }
        let source_head = policy
            .source_pull_request
            .as_ref()
            .map(|source| source.input.commit.clone());
        self.db.record_event(
            Some(task_id),
            Some(&attempt.id),
            actor.trim(),
            "publication.workflow_authorized",
            serde_json::json!({
                "attempt_record_sha256": artifact.sha256,
                "source_head": source_head,
                "repositories": repositories,
                "reason": reason.trim(),
            }),
        )?;
        self.retry_publication(task_id, actor.trim())
    }

    /// Explicit recovery of a validated source result; never invokes a new worker or promotes a dry run.
    pub fn retry_publication(&mut self, task_id: &str, actor: &str) -> Result<Task> {
        let mut task = self.db.get_task(task_id)?;
        let Some(source) = source_of(&task)? else {
            return self.retry_manual_publication(task_id, actor);
        };
        if task.state != TaskState::NeedsHuman
            || task.delivery_mode != DeliveryMode::MergeRequest
            || source.publisher_login.is_none()
        {
            return Err(invalid("only held publication tasks can retry publication"));
        }
        let attempt = self
            .db
            .latest_attempt(task_id)?
            .ok_or_else(|| invalid("missing attempt"))?;
        if attempt.state != AttemptState::Valid {
            return Err(invalid("retry requires a validated result"));
        }
        let record: AttemptRecord = self.artifacts.get_json(
            attempt
                .attempt_record
                .as_ref()
                .ok_or_else(|| invalid("missing record"))?,
        )?;
        let result: AgentResult = self.artifacts.get_json(
            attempt
                .result
                .as_ref()
                .ok_or_else(|| invalid("missing result"))?,
        )?;
        if !matches!(
            result.outcome,
            Outcome::Proposal | Outcome::ApproveUntouched
        ) {
            return Err(invalid(
                "agent needs a new decision, not a publication retry",
            ));
        }
        let mut tracked = self.db.get_pull_request_source(&source.tracking_id)?;
        if tracked.last_task_id.as_deref() != Some(task_id) {
            return Err(invalid("newer source evaluation exists"));
        }
        let job = self.load_job(&attempt)?;
        let policy: ResolvedProjectPolicy =
            serde_json::from_str(&task.policy_snapshot).map_err(|e| invalid(&e.to_string()))?;
        self.require_source_authority(&task, &policy)?;
        tracked.paused = false;
        self.db.save_pull_request_source(&tracked)?;
        self.db.record_event(
            Some(task_id),
            Some(&attempt.id),
            actor,
            "publication.retry_requested",
            serde_json::json!({}),
        )?;
        self.db.set_task_state(task_id, TaskState::Working, actor)?;
        task.state = TaskState::Working;
        if let Err(error) = self.publish_source_result(&task, &attempt, &job, &record, &result) {
            self.needs_human(
                &task,
                &attempt,
                &format!("publication retry withheld: {error}"),
            )?;
            self.persist_forge_backoffs()?;
            return Err(error);
        }
        self.persist_forge_backoffs()?;
        Ok(self.db.get_task(task_id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artidezi_config::model::ForgeProvider;
    use artidezi_contract::result::{
        Evidence, EvidenceKind, ResultRepository, RiskHint, RiskLevel,
    };

    #[test]
    fn evidence_source_handles_a_non_ascii_commit_segment() {
        let rendered = evidence_source(
            "https://forge.invalid/a/b/commit/aééééééééééééé",
            &repository("a/b"),
        );
        assert!(rendered.contains("aéééééééééé"));
    }

    fn repository(remote: &str) -> ResolvedRepository {
        ResolvedRepository {
            forge: "local".into(),
            forge_provider: ForgeProvider::Forgejo,
            forge_api_base_url: None,
            forge_bot_login: None,
            remote: remote.into(),
            branch: "main".into(),
            description: String::new(),
            access: RepositoryAccess::ReadWrite,
            setup_recipe: vec![],
            verification_recipe: vec![],
            allowed_paths: None,
            tagging: None,
        }
    }

    #[test]
    fn formats_local_and_foreign_commit_evidence_as_distinct_links() {
        let local = repository("example/app");
        assert_eq!(
            evidence_source(
                "https://forge.example/example/app/commit/abcdef0123456789",
                &local
            ),
            "[example/app](https://forge.example/example/app) @ [abcdef012345](https://forge.example/example/app/commit/abcdef0123456789)"
        );
        assert_eq!(
            evidence_source(
                "https://github.com/example/upstream/commit/abcdef0123456789",
                &local
            ),
            "[example/upstream](https://github.com/example/upstream) @ [abcdef012345](https://github.com/example/upstream/commit/abcdef0123456789)"
        );
    }

    #[test]
    fn review_evidence_stays_readable_and_collapsed_without_losing_items() {
        let mut result = AgentResult {
            schema_version: 1,
            outcome: Outcome::ApproveUntouched,
            summary: "Approve unchanged.".into(),
            risk_hint: Some(RiskHint {
                level: RiskLevel::Low,
                rationale: "The checked path is narrow.".into(),
            }),
            evidence: vec![Evidence {
                kind: EvidenceKind::LocalCheck,
                source: "git diff base...refs/artidezi/inputs/source-pr".into(),
                note: "The PR changes only `matrix_synapse_version`; [target evidence](https://example.invalid/commit/abc) confirms `device_id`. <b>not HTML</b>".into(),
            }],
            repositories: vec![ResultRepository {
                id: "role".into(),
                changed: false,
                head_commit: None,
                tag_proposal: None,
            }],
            questions_for_human: vec![],
            notes_for_next_attempt: None,
        };
        for index in 2..=9 {
            result.evidence.push(Evidence {
                kind: EvidenceKind::Other,
                source: format!("check {index}"),
                note: format!("Evidence {index}"),
            });
        }

        let body = evidence_section(&result, &repository("example/app"));
        assert!(body.starts_with("<details>\n<summary>Evidence (9 items)</summary>"));
        assert!(body.contains("`matrix_synapse_version`"));
        assert!(body.contains("[target evidence](https://example.invalid/commit/abc)"));
        assert!(body.contains("`device_id`"));
        assert!(body.contains("&lt;b&gt;not HTML&lt;/b&gt;"));
        assert!(body.contains("` git diff base...refs/artidezi/inputs/source-pr `"));
        assert!(
            body.contains("Evidence 9"),
            "collapsed output must retain every item"
        );
        assert!(body.ends_with("\n</details>\n"));
    }

    #[test]
    fn review_summary_labels_risk_and_questions_for_humans() {
        let result: AgentResult = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "outcome": "needs_human",
            "summary": "A decision is required.",
            "risk_hint": {"level": "medium", "rationale": "Compatibility is uncertain."},
            "repositories": [{"id": "role", "changed": false}],
            "questions_for_human": ["Should `legacy_option` remain supported?"]
        }))
        .unwrap();
        let body = summary(&result);
        assert!(body.contains("**Agent-assessed risk: Medium.**"));
        assert!(
            body.contains("### Maintainer question\n\n- Should `legacy_option` remain supported?")
        );
    }
}
