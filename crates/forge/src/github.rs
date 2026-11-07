// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Read-only GitHub inspection and App-authenticated scheduled intake. `GithubReader` supports
//! standalone inspection; `GithubForge` provides the scheduler adapter and scoped Git credentials.

use crate::{ForgeError, Result};
use std::sync::Arc;

mod app;
mod ci_logs;
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixture;
mod http;
pub use app::{GithubAppIdentity, GithubForge};
use serde::{Deserialize, Serialize};

const API_VERSION: &str = "2026-03-10";
const MAX_PAGES: usize = 10;

pub struct GithubReader {
    token: Option<String>,
    http: Arc<http::Http>,
    conditional: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Repository {
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    pub disabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Author {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PullRef {
    #[serde(rename = "ref")]
    pub name: String,
    pub sha: String,
    pub repo: Option<RepositoryIdentity>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RepositoryIdentity {
    pub full_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub user: Author,
    pub state: String,
    pub merged: bool,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    pub draft: bool,
    pub head: PullRef,
    pub base: PullRef,
    pub changed_files: usize,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub head_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CommitStatus {
    pub context: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct CombinedStatus {
    sha: String,
    state: String,
    total_count: usize,
    statuses: Vec<CommitStatus>,
}

#[derive(Deserialize)]
struct CheckPage {
    total_count: usize,
    check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub previous_filename: Option<String>,
    /// GitHub may omit or truncate a patch; this is not a complete Git snapshot.
    pub patch: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub schema_version: u32,
    pub observed_at: String,
    pub repository: Repository,
    pub source: PullRequest,
    pub source_url: String,
    pub source_ref: String,
    pub statuses: Vec<CommitStatus>,
    pub check_runs: Vec<CheckRun>,
    pub checks_summary: &'static str,
    pub changed_files: Vec<ChangedFile>,
    pub notes: Vec<&'static str>,
}

impl GithubReader {
    /// Optional token is supplied explicitly, never read from Git/CLI environment credentials.
    /// GitHub App installation tokens and scoped read-only tokens can be used for a one-shot probe;
    /// automatic App token issuance/renewal belongs to the future authenticated adapter.
    pub fn new(token: Option<String>) -> Self {
        Self::at("https://api.github.com", token)
    }

    fn at(api: &str, token: Option<String>) -> Self {
        Self {
            token,
            http: Arc::new(http::Http::new(api)),
            conditional: false,
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.http.get(path, self.token.as_deref(), self.conditional)
    }

    fn checks(
        &self,
        remote: &str,
        head: &str,
    ) -> Result<(Vec<CommitStatus>, Vec<CheckRun>, &'static str)> {
        let root = format!("/repos/{remote}");
        let mut statuses = Vec::new();
        let mut status_state = String::new();
        let mut expected = None;
        for page in 1..=MAX_PAGES {
            let batch: CombinedStatus = self.get(&format!(
                "{root}/commits/{head}/status?per_page=100&page={page}"
            ))?;
            if batch.sha != head
                || expected.is_some_and(|count| count != batch.total_count)
                || (!status_state.is_empty() && status_state != batch.state)
            {
                return Err(ForgeError::Other(
                    "GitHub commit statuses changed during inspection; retry".into(),
                ));
            }
            status_state = batch.state;
            expected = Some(batch.total_count);
            let empty = batch.statuses.is_empty();
            statuses.extend(batch.statuses);
            if statuses.len() == batch.total_count {
                break;
            }
            if empty || statuses.len() > batch.total_count || page == MAX_PAGES {
                return Err(ForgeError::Other(
                    "GitHub commit-status listing is incomplete".into(),
                ));
            }
        }
        let mut check_runs = Vec::new();
        let mut expected = None;
        for page in 1..=MAX_PAGES {
            let batch: CheckPage = self.get(&format!(
                "{root}/commits/{head}/check-runs?filter=latest&per_page=100&page={page}"
            ))?;
            if expected.is_some_and(|count| count != batch.total_count)
                || batch.check_runs.iter().any(|run| run.head_sha != head)
            {
                return Err(ForgeError::Other(
                    "GitHub check runs changed during inspection; retry".into(),
                ));
            }
            expected = Some(batch.total_count);
            let empty = batch.check_runs.is_empty();
            check_runs.extend(batch.check_runs);
            if check_runs.len() == batch.total_count {
                break;
            }
            if empty || check_runs.len() > batch.total_count || page == MAX_PAGES {
                return Err(ForgeError::Other(
                    "GitHub check-run listing is incomplete".into(),
                ));
            }
        }
        if statuses
            .iter()
            .map(|s| &s.context)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != statuses.len()
            || check_runs
                .iter()
                .map(|r| r.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != check_runs.len()
        {
            return Err(ForgeError::Other(
                "GitHub checks changed during pagination; duplicate records found".into(),
            ));
        }
        let summary = summarize_checks(&status_state, &statuses, &check_runs);
        Ok((statuses, check_runs, summary))
    }

    pub fn inspect(&self, remote: &str, number: u64) -> Result<Inspection> {
        validate_remote(remote)?;
        if number == 0 {
            return Err(ForgeError::Other(
                "pull request number must be positive".into(),
            ));
        }
        let root = format!("/repos/{remote}");
        let repository: Repository = self.get(&root)?;
        if !repository.full_name.eq_ignore_ascii_case(remote) {
            return Err(ForgeError::Other(
                "GitHub repository identity changed".into(),
            ));
        }
        let source: PullRequest = self.get(&format!("{root}/pulls/{number}"))?;
        validate_source(&source, remote, number)?;
        let head = &source.head.sha;
        let (statuses, check_runs, checks_summary) = self.checks(remote, head)?;
        let mut changed_files = Vec::new();
        for page in 1..=MAX_PAGES {
            let batch: Vec<ChangedFile> = self.get(&format!(
                "{root}/pulls/{number}/files?per_page=100&page={page}"
            ))?;
            let empty = batch.is_empty();
            changed_files.extend(batch);
            if changed_files.len() == source.changed_files {
                break;
            }
            if empty || changed_files.len() > source.changed_files || page == MAX_PAGES {
                return Err(ForgeError::Other(
                    "GitHub changed-file listing is incomplete".into(),
                ));
            }
        }
        if statuses
            .iter()
            .map(|s| &s.context)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != statuses.len()
            || check_runs
                .iter()
                .map(|r| r.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != check_runs.len()
            || changed_files
                .iter()
                .map(|f| &f.filename)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != changed_files.len()
        {
            return Err(ForgeError::Other(
                "GitHub listing changed during pagination; duplicate records found".into(),
            ));
        }
        let current: PullRequest = self.get(&format!("{root}/pulls/{number}"))?;
        validate_source(&current, remote, number)?;
        if current.head.sha != source.head.sha
            || current.base.sha != source.base.sha
            || current.base.name != source.base.name
            || current.state != source.state
            || current.merged != source.merged
            || current.draft != source.draft
            || current.changed_files != source.changed_files
            || current.body != source.body
            || current.title != source.title
            || current.user.login != source.user.login
        {
            return Err(ForgeError::Other(
                "GitHub source changed during inspection; retry".into(),
            ));
        }
        Ok(Inspection {
            schema_version: 1,
            observed_at: chrono::Utc::now().to_rfc3339(),
            repository,
            source,
            source_url: format!("https://github.com/{remote}/pull/{number}"),
            source_ref: format!("refs/pull/{number}/head"),
            statuses,
            check_runs,
            checks_summary,
            changed_files,
            notes: vec![
                "Read-only inspection: no task, review, approval, push or merge was created.",
                "Reported checks are not required-check or branch-protection validation, nor an approval verdict.",
                "Check runs may change after this snapshot; GitHub limits the check suites exposed by this endpoint.",
                "API patches can be omitted or truncated; use a verified Git snapshot for agent work.",
            ],
        })
    }
}

fn validate_remote(remote: &str) -> Result<()> {
    let valid = remote.split_once('/').is_some_and(|(owner, repo)| {
        [owner, repo].iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    });
    if !valid {
        return Err(ForgeError::Other(
            "repository must be owner/name, without a URL or query string".into(),
        ));
    }
    Ok(())
}

fn validate_source(source: &PullRequest, remote: &str, number: u64) -> Result<()> {
    validate_pull_identity(source.number, &source.head, &source.base, remote, number)
}

fn validate_pull_identity(
    actual_number: u64,
    head: &PullRef,
    base: &PullRef,
    remote: &str,
    number: u64,
) -> Result<()> {
    if actual_number != number
        || !base
            .repo
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(remote))
        || [&head.sha, &base.sha]
            .iter()
            .any(|sha| sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(ForgeError::Other(
            "GitHub source identity or commit SHA is invalid".into(),
        ));
    }
    Ok(())
}

fn summarize_checks(combined: &str, statuses: &[CommitStatus], runs: &[CheckRun]) -> &'static str {
    if (!statuses.is_empty() && combined != "success") && combined != "failure"
        || statuses
            .iter()
            .any(|s| !matches!(s.state.as_str(), "success" | "failure" | "error"))
        || runs.iter().any(|r| {
            r.status != "completed"
                || !matches!(
                    r.conclusion.as_deref(),
                    Some(
                        "success"
                            | "neutral"
                            | "skipped"
                            | "failure"
                            | "cancelled"
                            | "timed_out"
                            | "action_required"
                            | "stale"
                    )
                )
        })
    {
        return "pending";
    }
    if (combined == "failure" && !statuses.is_empty())
        || statuses
            .iter()
            .any(|s| matches!(s.state.as_str(), "failure" | "error"))
        || runs.iter().any(|r| {
            matches!(
                r.conclusion.as_deref(),
                Some("failure" | "cancelled" | "timed_out" | "action_required" | "stale")
            )
        })
    {
        return "failure";
    }
    if !statuses.is_empty()
        || runs
            .iter()
            .any(|r| r.conclusion.as_deref() == Some("success"))
    {
        "success"
    } else if !runs.is_empty() {
        "inconclusive"
    } else {
        "absent"
    }
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
