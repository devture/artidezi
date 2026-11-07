// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Structural and cross-document validation rules. These run in the supervisor for early feedback
//! and again, unconditionally, in the orchestrator.

use std::collections::BTreeSet;

use regex::Regex;

use crate::common::{Outcome, RepositoryAccess, SCHEMA_VERSION};
use crate::job::{EgressMode, Job, PrivateNetworkMode, WebSearchScope};
use crate::result::AgentResult;

/// One human-readable problem. Validation collects every problem instead of stopping at the first
/// so an attempt record can show all of them.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Problem(pub String);

fn problem(msg: impl Into<String>) -> Problem {
    Problem(msg.into())
}

/// Checks a job for internal consistency before it is dispatched or accepted by a worker.
/// Ids become path components on workers.
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub fn validate_job(job: &Job) -> Vec<Problem> {
    let mut problems = Vec::new();

    if !is_plain_id(job.attempt_id.as_str()) {
        problems.push(problem("attempt_id must be a plain name"));
    }
    for repo in &job.repositories {
        if !is_plain_id(repo.id.as_str()) {
            problems.push(problem(format!(
                "repository id {} must be a plain name",
                repo.id
            )));
        }
        for input in &repo.input_refs {
            if !is_plain_id(&input.name) {
                problems.push(problem(format!(
                    "repository {}: input ref {} must be a plain name",
                    repo.id, input.name
                )));
            }
        }
    }

    if !crate::job::SUPPORTED_JOB_SCHEMA_VERSIONS.contains(&job.schema_version) {
        problems.push(problem(format!(
            "job schema_version {} is unsupported",
            job.schema_version
        )));
    }
    if job.attempt_number == 0 {
        problems.push(problem("attempt_number must start at 1"));
    }
    if job.schema_version == 1 && job.attempt_number > 1 && job.previous_attempt.is_none() {
        problems.push(problem("a revision must carry previous_attempt"));
    }
    if job.attempt_number == 1 && job.previous_attempt.is_some() {
        problems.push(problem("the first attempt cannot carry previous_attempt"));
    }
    if job.allowed_outcomes.is_empty() {
        problems.push(problem("allowed_outcomes must not be empty"));
    }
    if !job.task_document.is_well_formed() {
        problems.push(problem("task_document digest is malformed"));
    }

    validate_network(job, &mut problems);

    let mut seen = BTreeSet::new();
    for repo in &job.repositories {
        if !seen.insert(&repo.id) {
            problems.push(problem(format!(
                "repository id {} is declared twice",
                repo.id
            )));
        }
        if repo.branch.is_empty() {
            problems.push(problem(format!(
                "repository {}: branch is mandatory",
                repo.id
            )));
        }
        if !repo.base_commit.is_well_formed() {
            problems.push(problem(format!(
                "repository {}: base_commit is malformed",
                repo.id
            )));
        }
        if !repo.snapshot_bundle.is_well_formed() {
            problems.push(problem(format!(
                "repository {}: snapshot_bundle digest is malformed",
                repo.id
            )));
        }
        let mut ref_names = BTreeSet::new();
        for input in &repo.input_refs {
            if input.name.is_empty() || input.name.contains('/') || input.name.contains("..") {
                problems.push(problem(format!(
                    "repository {}: input ref name {:?} is not a plain name",
                    repo.id, input.name
                )));
            }
            if !ref_names.insert(&input.name) {
                problems.push(problem(format!(
                    "repository {}: input ref {} is declared twice",
                    repo.id, input.name
                )));
            }
            if !input.commit.is_well_formed() {
                problems.push(problem(format!(
                    "repository {}: input ref {} commit is malformed",
                    repo.id, input.name
                )));
            }
        }
        for step in repo.setup_recipe.iter().chain(&repo.verification_recipe) {
            if step.command.is_empty() {
                problems.push(problem(format!(
                    "repository {}: recipe step {} has an empty command",
                    repo.id, step.name
                )));
            }
            if step.cwd.starts_with('/') || step.cwd.split('/').any(|c| c == "..") {
                problems.push(problem(format!(
                    "repository {}: recipe step {} cwd must stay inside the repository",
                    repo.id, step.name
                )));
            }
        }
        if let Some(tagging) = &repo.tagging {
            if repo.access == RepositoryAccess::ReadOnly {
                problems.push(problem(format!(
                    "repository {}: read_only repositories cannot carry a tagging policy",
                    repo.id
                )));
            }
            if Regex::new(&tagging.allowed_pattern).is_err() {
                problems.push(problem(format!(
                    "repository {}: tagging allowed_pattern is not a valid regular expression",
                    repo.id
                )));
            }
        }
    }

    if job.allowed_outcomes.contains(&Outcome::Proposal)
        && job.read_write_repositories().next().is_none()
    {
        problems.push(problem(
            "proposal is an allowed outcome but no repository is read_write",
        ));
    }

    problems
}

fn validate_network(job: &Job, problems: &mut Vec<Problem>) {
    let egress = &job.network.command_egress;
    match (egress.mode, egress.allowed_domains.is_some()) {
        (EgressMode::DomainAllowlist, false) => {
            problems.push(problem(
                "command_egress.allowed_domains is mandatory in domain_allowlist mode",
            ));
        }
        (EgressMode::DomainAllowlist, true) => {}
        (_, true) => problems.push(problem(
            "command_egress.allowed_domains is only allowed in domain_allowlist mode",
        )),
        (_, false) => {}
    }
    match (egress.private_network, egress.allowed_private_endpoints.is_empty()) {
        (PrivateNetworkMode::DomainAllowlist, true) => problems.push(problem(
            "command_egress.allowed_private_endpoints is mandatory when private_network is domain_allowlist",
        )),
        (PrivateNetworkMode::Blocked, false) => problems.push(problem(
            "command_egress.allowed_private_endpoints must be empty when private_network is blocked",
        )),
        _ => {}
    }
    let search = &job.network.web_search;
    match (search.domain_scope, search.allowed_domains.is_some()) {
        (WebSearchScope::DomainAllowlist, false) => {
            problems.push(problem(
                "web_search.allowed_domains is mandatory in domain_allowlist scope",
            ));
        }
        (WebSearchScope::PublicInternet, true) => {
            problems.push(problem(
                "web_search.allowed_domains is only allowed in domain_allowlist scope",
            ));
        }
        _ => {}
    }
}

/// Checks a result document against the job it answers. Does not look at the collected commits;
/// that comparison happens where the collection is available.
pub fn validate_result(job: &Job, result: &AgentResult) -> Vec<Problem> {
    let mut problems = Vec::new();

    if result.schema_version != SCHEMA_VERSION {
        problems.push(problem(format!(
            "result schema_version {} is not the supported {SCHEMA_VERSION}",
            result.schema_version
        )));
    }
    if !job.allowed_outcomes.contains(&result.outcome) {
        problems.push(problem(format!(
            "outcome {:?} is not among the job's allowed outcomes",
            result.outcome
        )));
    }
    if result.summary.trim().is_empty() {
        problems.push(problem("summary must not be empty"));
    }

    let expected: BTreeSet<_> = job.read_write_repositories().map(|r| &r.id).collect();
    let mut seen = BTreeSet::new();
    let mut any_changed = false;
    for entry in &result.repositories {
        if !seen.insert(&entry.id) {
            problems.push(problem(format!(
                "repository {} is reported twice",
                entry.id
            )));
            continue;
        }
        let Some(repo) = job.repository(&entry.id) else {
            problems.push(problem(format!(
                "repository {} is not part of the job",
                entry.id
            )));
            continue;
        };
        if repo.access != RepositoryAccess::ReadWrite {
            problems.push(problem(format!(
                "repository {} is read_only and must not be reported",
                entry.id
            )));
            continue;
        }
        any_changed |= entry.changed;
        match (&entry.head_commit, entry.changed) {
            (None, true) => problems.push(problem(format!(
                "repository {}: head_commit is required when changed",
                entry.id
            ))),
            (Some(sha), _) if !sha.is_well_formed() => problems.push(problem(format!(
                "repository {}: head_commit is malformed",
                entry.id
            ))),
            _ => {}
        }
        if let Some(tag) = &entry.tag_proposal {
            match &repo.tagging {
                Some(policy) if policy.agent_may_propose => {
                    let anchored = format!("^(?:{})$", policy.allowed_pattern);
                    match Regex::new(&anchored) {
                        Ok(re) if re.is_match(tag) => {}
                        _ => problems.push(problem(format!(
                            "repository {}: tag proposal {tag:?} does not match the allowed pattern",
                            entry.id
                        ))),
                    }
                    if !entry.changed {
                        problems.push(problem(format!(
                            "repository {}: a tag proposal requires a change",
                            entry.id
                        )));
                    }
                }
                _ => problems.push(problem(format!(
                    "repository {}: tag proposals are not permitted",
                    entry.id
                ))),
            }
        }
    }
    for missing in expected.difference(&seen) {
        problems.push(problem(format!(
            "read_write repository {missing} is missing from the result"
        )));
    }

    match result.outcome {
        Outcome::Proposal if !any_changed => {
            problems.push(problem("proposal requires at least one changed repository"));
        }
        Outcome::ApproveUntouched | Outcome::NeedsHuman | Outcome::Failed if any_changed => {
            problems.push(problem(format!(
                "outcome {:?} requires every repository to be unchanged",
                result.outcome
            )));
        }
        _ => {}
    }
    match (result.outcome, result.questions_for_human.is_empty()) {
        (Outcome::NeedsHuman, true) => {
            problems.push(problem(
                "needs_human requires at least one question for a human",
            ));
        }
        (Outcome::NeedsHuman, false) => {}
        (_, false) => problems.push(problem(
            "questions_for_human must be empty unless the outcome is needs_human",
        )),
        (_, true) => {}
    }

    problems
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::common::{ArtifactRef, CommitSha};
    use crate::job::*;
    use crate::result::*;

    fn digest(c: char) -> ArtifactRef {
        ArtifactRef {
            sha256: std::iter::repeat_n(c, 64).collect(),
            size: 1,
        }
    }

    fn sha(c: char) -> CommitSha {
        CommitSha(std::iter::repeat_n(c, 40).collect())
    }

    fn repo(id: &str, access: RepositoryAccess) -> JobRepository {
        JobRepository {
            id: id.into(),
            access,
            branch: "main".into(),
            base_commit: sha('0'),
            snapshot_bundle: digest('a'),
            input_refs: vec![],
            setup_recipe: vec![],
            verification_recipe: vec![],
            allowed_paths: None,
            tagging: Some(TaggingPolicy {
                agent_may_propose: true,
                allowed_pattern: r"v[0-9]+\.[0-9]+\.[0-9]+-[0-9]+".into(),
            }),
        }
    }

    fn job() -> Job {
        Job {
            schema_version: SCHEMA_VERSION,
            attempt_id: "att_1".into(),
            task_id: "tsk_1".into(),
            attempt_number: 1,
            prompt_template_version: 1,
            agent_profile: AgentProfile {
                name: "fake".into(),
                harness: Harness::Fake,
                harness_version_constraint: "*".into(),
                model: "none".into(),
                model_settings: BTreeMap::new(),
                model_endpoint: ModelEndpointRef {
                    auth_mode: "none".into(),
                    credential_ref: "none".into(),
                    base_url: None,
                },
            },
            worker_requirements: WorkerRequirements {
                harness_sandbox: HarnessSandbox::Disabled,
                required_tags: vec![],
            },
            network: NetworkPolicy {
                command_egress: CommandEgress {
                    mode: EgressMode::PublicInternet,
                    allowed_domains: None,
                    denied_domains: vec![],
                    private_network: PrivateNetworkMode::Blocked,
                    allowed_private_endpoints: vec![],
                },
                web_search: WebSearch {
                    mode: WebSearchMode::Live,
                    domain_scope: WebSearchScope::PublicInternet,
                    allowed_domains: None,
                },
            },
            limits: Limits {
                wall_clock_seconds: 60,
                result_json_max_bytes: 1024,
                max_commits_per_repository: 5,
                max_diff_bytes_per_repository: 1024,
                max_commit_message_bytes: 256,
            },
            allowed_outcomes: vec![Outcome::Proposal, Outcome::NeedsHuman, Outcome::Failed],
            repositories: vec![
                repo("rw", RepositoryAccess::ReadWrite),
                JobRepository {
                    tagging: None,
                    ..repo("ro", RepositoryAccess::ReadOnly)
                },
            ],
            task_document: digest('b'),
            previous_attempt: None,
        }
    }

    fn proposal() -> AgentResult {
        AgentResult {
            schema_version: SCHEMA_VERSION,
            outcome: Outcome::Proposal,
            summary: "did a thing".into(),
            risk_hint: None,
            evidence: vec![],
            repositories: vec![ResultRepository {
                id: "rw".into(),
                changed: true,
                head_commit: Some(sha('1')),
                tag_proposal: Some("v1.2.3-0".into()),
            }],
            questions_for_human: vec![],
            notes_for_next_attempt: None,
        }
    }

    #[test]
    fn revision_without_artifacts_requires_job_version_two() {
        let mut revision = job();
        revision.attempt_number = 2;
        assert!(
            validate_job(&revision)
                .iter()
                .any(|p| p.0.contains("previous_attempt"))
        );
        revision.schema_version = crate::job::JOB_SCHEMA_VERSION;
        assert!(validate_job(&revision).is_empty());
        revision.schema_version = crate::job::JOB_SCHEMA_VERSION + 1;
        assert!(
            validate_job(&revision)
                .iter()
                .any(|p| p.0.contains("unsupported"))
        );
    }

    #[test]
    fn well_formed_job_and_result_pass() {
        assert!(validate_job(&job()).is_empty());
        assert!(validate_result(&job(), &proposal()).is_empty());
    }

    #[test]
    fn allowlist_mode_requires_domains() {
        let mut j = job();
        j.network.command_egress.mode = EgressMode::DomainAllowlist;
        assert_eq!(validate_job(&j).len(), 1);
        j.network.command_egress.allowed_domains = Some(vec!["example.org".into()]);
        assert!(validate_job(&j).is_empty());
        j.network.command_egress.mode = EgressMode::Blocked;
        assert_eq!(validate_job(&j).len(), 1);
    }

    #[test]
    fn outcome_must_match_changes() {
        let mut r = proposal();
        r.repositories[0].changed = false;
        r.repositories[0].tag_proposal = None;
        let problems = validate_result(&job(), &r);
        assert!(problems.iter().any(|p| p.0.contains("proposal requires")));

        let mut r = proposal();
        r.outcome = Outcome::NeedsHuman;
        r.questions_for_human = vec!["which?".into()];
        let problems = validate_result(&job(), &r);
        assert!(
            problems
                .iter()
                .any(|p| p.0.contains("requires every repository to be unchanged"))
        );
    }

    #[test]
    fn tag_proposal_is_policed() {
        let mut r = proposal();
        r.repositories[0].tag_proposal = Some("1.2.3".into());
        assert!(
            validate_result(&job(), &r)
                .iter()
                .any(|p| p.0.contains("allowed pattern"))
        );

        let mut j = job();
        j.repositories[0].tagging = None;
        assert!(
            validate_result(&j, &proposal())
                .iter()
                .any(|p| p.0.contains("not permitted"))
        );
    }

    #[test]
    fn read_only_and_unknown_repositories_are_rejected() {
        let mut r = proposal();
        r.repositories.push(ResultRepository {
            id: "ro".into(),
            changed: false,
            head_commit: None,
            tag_proposal: None,
        });
        assert!(
            validate_result(&job(), &r)
                .iter()
                .any(|p| p.0.contains("read_only"))
        );

        let mut r = proposal();
        r.repositories[0].id = "nope".into();
        let problems = validate_result(&job(), &r);
        assert!(problems.iter().any(|p| p.0.contains("not part of the job")));
        assert!(
            problems
                .iter()
                .any(|p| p.0.contains("missing from the result"))
        );
    }

    #[test]
    fn disallowed_outcome_is_rejected() {
        let mut r = proposal();
        r.outcome = Outcome::ApproveUntouched;
        r.repositories[0].changed = false;
        r.repositories[0].head_commit = None;
        r.repositories[0].tag_proposal = None;
        assert!(
            validate_result(&job(), &r)
                .iter()
                .any(|p| p.0.contains("allowed outcomes"))
        );
    }

    #[test]
    fn documents_round_trip_through_json() {
        let j = job();
        let text = serde_json::to_string_pretty(&j).unwrap();
        assert_eq!(serde_json::from_str::<Job>(&text).unwrap(), j);
        let r = proposal();
        let text = serde_json::to_string_pretty(&r).unwrap();
        assert_eq!(serde_json::from_str::<AgentResult>(&text).unwrap(), r);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut value = serde_json::to_value(proposal()).unwrap();
        value["delivery_mode"] = serde_json::json!("direct_push_fast_forward");
        assert!(serde_json::from_value::<AgentResult>(value).is_err());
    }
}
