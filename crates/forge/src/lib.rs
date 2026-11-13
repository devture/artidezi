// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral forge access. The orchestrator is the only party that ever talks to a forge;
//! the agent gets snapshots without remotes.
//!
//! Vocabulary is provider-neutral: merge request, not pull request.

mod ci_log;
pub mod fake;
pub mod forgejo;
pub mod github;

use artidezi_contract::common::CommitSha;
use artidezi_git::cache::HttpsCredentials;

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("GitHub credential was rejected or expired; verify the App installation and key")]
    AuthenticationRejected,
    #[error("GitHub permission/rate limit: retry after {retry_at}")]
    RetryAfter {
        retry_at: chrono::DateTime<chrono::Utc>,
    },
    #[error("{entity} {id} not found")]
    NotFound { entity: &'static str, id: String },
    #[error("head moved: expected {expected}, found {actual}")]
    HeadMoved {
        expected: CommitSha,
        actual: CommitSha,
    },
    #[error("merge request {0} is not mergeable: {1}")]
    NotMergeable(u64, String),
    #[error("git error: {0}")]
    Git(#[from] artidezi_git::GitError),
    #[error("libgit2 error: {0}")]
    Git2(#[from] git2::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ForgeError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRequestState {
    Open,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    /// No commit statuses or check runs have been reported for this head.
    Absent,
    /// At least one reported check is still running.
    Pending,
    /// All reported checks finished, but none passed or failed (for example, all were skipped).
    Inconclusive,
    Success,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckResultKind {
    CommitStatus,
    CheckRun,
}

/// One provider-reported result associated with the merge-request head. Names and states are
/// untrusted repository/forge data; consumers must not treat them as instructions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub kind: CheckResultKind,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

/// A bounded, sanitized excerpt from one exact Actions job. Its text remains untrusted data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiJobLog {
    pub name: String,
    pub job_id: Option<u64>,
    pub run_id: Option<u64>,
    pub run_attempt: Option<u64>,
    pub outcome: CiLogOutcome,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub raw_bytes: u64,
    pub sanitized_bytes: u64,
    pub truncated: bool,
    pub excerpt: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiLogOutcome {
    Available,
    Pending,
    UnsupportedProvider,
    PermissionDenied,
    MissingOrInaccessible,
    TooLarge,
    FetchFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestInfo {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub body: String,
    pub author_login: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_commit: CommitSha,
    /// The commit a merge produced on the target branch, once merged.
    pub merge_commit: Option<CommitSha>,
    pub state: MergeRequestState,
    pub draft: bool,
    pub checks: ChecksState,
    /// Complete normalized result names/states when the adapter supports them. `Some([])` means the
    /// provider was queried successfully but reported no results; `None` means details unavailable.
    pub check_results: Option<Vec<CheckResult>>,
}

/// Fields sufficient to route an open pull request before fetching its mutable details and checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestSummary {
    pub number: u64,
    pub title: String,
    pub author_login: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_commit: CommitSha,
    pub draft: bool,
}

impl From<&MergeRequestInfo> for MergeRequestSummary {
    fn from(pull: &MergeRequestInfo) -> Self {
        Self {
            number: pull.number,
            title: pull.title.clone(),
            author_login: pull.author_login.clone(),
            source_branch: pull.source_branch.clone(),
            target_branch: pull.target_branch.clone(),
            head_commit: pull.head_commit.clone(),
            draft: pull.draft,
        }
    }
}

/// A discovery listing may include full details, or defer them until a source needs inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestListing {
    pub summary: MergeRequestSummary,
    pub details: Option<MergeRequestInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMergeRequest {
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub draft: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    /// Fast-forward or merge commit as the forge decides; used for untouched Renovate merges.
    Merge,
    /// Rebase-and-merge, preserving individual commits.
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeRequestReview {
    #[serde(default)]
    pub state: ReviewState,
    pub id: u64,
    pub author_login: String,
    pub head_commit: CommitSha,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    #[default]
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
    Other,
}

/// The exact open proposal a caller intends to close.
#[derive(Clone, Copy, Debug)]
pub struct ExpectedProposal<'a> {
    pub number: u64,
    pub source_branch: &'a str,
    pub target_branch: &'a str,
    pub head: &'a CommitSha,
}
impl ExpectedProposal<'_> {
    /// Whether `info` is still this bot's unchanged, open proposal.
    pub fn matches(&self, info: &MergeRequestInfo, bot_login: &str) -> bool {
        info.number == self.number
            && info.author_login == bot_login
            && self.source_branch.starts_with("artidezi/")
            && info.source_branch == self.source_branch
            && info.target_branch == self.target_branch
            && info.head_commit == *self.head
            && info.state == MergeRequestState::Open
    }
}

pub trait ForgeAdapter: Send + Sync {
    /// Repository paths that define what this forge's CI runs. Publishing a change under any of
    /// them needs human authorization. Required, so that a new adapter has to decide.
    fn protected_paths(&self) -> &'static [&'static str];
    /// Whether `result` is a failure whose job log `ci_job_logs` may be able to collect: a failed
    /// check run by default.
    fn ci_log_candidate(&self, result: &CheckResult) -> bool {
        result.kind == CheckResultKind::CheckRun
            && result.status == "completed"
            && result.conclusion.as_deref() == Some("failure")
    }
    /// Optional exact-head log collection for failed result names chosen by `ci_log_candidate`.
    /// A provider may return unavailable outcomes; failure never grants a publication or CI-check
    /// capability.
    fn ci_job_logs(&self, _remote: &str, _head: &CommitSha, names: &[String]) -> Vec<CiJobLog> {
        names
            .iter()
            .map(|name| CiJobLog {
                name: name.clone(),
                job_id: None,
                run_id: None,
                run_attempt: None,
                outcome: CiLogOutcome::UnsupportedProvider,
                captured_at: chrono::Utc::now(),
                raw_bytes: 0,
                sanitized_bytes: 0,
                truncated: false,
                excerpt: None,
            })
            .collect()
    }
    fn create_approval_review(
        &self,
        _remote: &str,
        _number: u64,
        _head: &CommitSha,
        _body: &str,
    ) -> Result<MergeRequestReview> {
        Err(ForgeError::Other("approval reviews are unsupported".into()))
    }

    /// Explicit provider capability, separate from project/agent permissions. Deny by default.
    fn writes_allowed(&self) -> bool {
        false
    }
    fn writes_allowed_for(&self, _remote: &str) -> bool {
        self.writes_allowed()
    }
    /// Fetch credentials remain read-only on providers with scoped installation tokens.
    fn git_push_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        self.git_credentials(remote)
    }
    /// A separate credential for a human-authorized proposal that changes protected paths.
    /// Providers must opt in; ordinary publication credentials stay narrower.
    fn git_push_workflow_credentials(&self, _remote: &str) -> Result<Option<HttpsCredentials>> {
        Err(ForgeError::Other(
            "workflow-file publication credentials are unsupported".into(),
        ))
    }
    fn branch_head_optional(&self, remote: &str, branch: &str) -> Result<Option<CommitSha>> {
        match self.branch_head(remote, branch) {
            Ok(head) => Ok(Some(head)),
            Err(ForgeError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn list_reviews(&self, _remote: &str, _number: u64) -> Result<Vec<MergeRequestReview>> {
        Err(ForgeError::Other("comment reviews are unsupported".into()))
    }
    /// All proposals on this source branch, including closed/merged ones, for retry reconciliation.
    fn proposals_for_branch(&self, _remote: &str, _branch: &str) -> Result<Vec<MergeRequestInfo>> {
        Err(ForgeError::Other(
            "proposal reconciliation is unsupported".into(),
        ))
    }
    /// Informational COMMENT only. Approval and change-request authority are separate features.
    fn create_comment_review(
        &self,
        _remote: &str,
        _number: u64,
        _head: &CommitSha,
        _body: &str,
    ) -> Result<MergeRequestReview> {
        Err(ForgeError::Other("comment reviews are unsupported".into()))
    }
    /// Server-advised backoff, persisted by the discovery controller across restarts.
    fn retry_after(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        None
    }
    /// URL the orchestrator's git cache fetches from and pushes to.
    fn clone_url(&self, remote: &str) -> String;
    /// Short-lived credentials for git over HTTPS, if the forge needs them.
    fn git_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>>;
    fn branch_head(&self, remote: &str, branch: &str) -> Result<CommitSha>;
    /// Ref fetched from the configured target remote, including PRs whose heads live in forks.
    fn merge_request_ref(&self, remote: &str, number: u64) -> Result<String>;
    fn create_merge_request(
        &self,
        remote: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo>;
    fn update_merge_request(
        &self,
        remote: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<()>;
    /// Comments on and closes exactly the expected open proposal the bot opened from an
    /// `artidezi/` branch, refusing before any write if it differs.
    fn close_merge_request(
        &self,
        _remote: &str,
        _expected: &ExpectedProposal<'_>,
        _comment: &str,
    ) -> Result<()> {
        Err(ForgeError::Other("closing proposals is unsupported".into()))
    }
    fn get_merge_request(&self, remote: &str, number: u64) -> Result<MergeRequestInfo>;
    /// Complete open listing for discovery. Providers may avoid fetching details and checks here;
    /// the caller must fetch a full merge request before preparing new work.
    fn list_open_merge_request_summaries(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestListing>> {
        self.list_open_merge_requests(remote, author).map(|pulls| {
            pulls
                .into_iter()
                .map(|details| MergeRequestListing {
                    summary: MergeRequestSummary::from(&details),
                    details: Some(details),
                })
                .collect()
        })
    }
    /// Open merge requests, optionally filtered by author login.
    fn list_open_merge_requests(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestInfo>>;
    /// Merges only if the merge request's head still equals `expected_head`. Returns the resulting
    /// commit on the target branch.
    fn merge(
        &self,
        remote: &str,
        number: u64,
        expected_head: &CommitSha,
        method: MergeMethod,
    ) -> Result<CommitSha>;
    /// Creates a new tag. Never moves or deletes an existing one.
    /// The commit an existing tag points at. Providers that can't create tags have none.
    fn tag_commit(&self, _remote: &str, _name: &str) -> Result<Option<CommitSha>> {
        Ok(None)
    }
    fn create_tag(&self, remote: &str, name: &str, commit: &CommitSha) -> Result<()>;
}
