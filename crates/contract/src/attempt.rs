// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! `attempt.json`: what the worker supervisor observed and collected for one attempt.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{ArtifactRef, AttemptId, CommitSha, RepositoryId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub schema_version: u32,
    pub attempt_id: AttemptId,
    /// The `job.json` this record answers.
    pub job: ArtifactRef,
    pub status: AttemptStatus,
    pub supervisor: SupervisorInfo,
    pub harness: HarnessRecord,
    /// Absent when no result document could be obtained at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRecord>,
    pub repositories: Vec<RepositoryCollection>,
    /// Every artifact uploaded for this attempt, by name.
    #[serde(default)]
    pub artifacts: Vec<ArtifactEntry>,
}

/// How the attempt ended from the supervisor's point of view. Independent of the agent's outcome
/// and of result validation problems, which live in [`ResultRecord::validation_errors`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// The harness exited and the record is complete.
    Completed,
    /// The harness failed to start or crashed.
    HarnessError,
    /// Wall clock exceeded; the harness was killed.
    Timeout,
    /// The supervisor could not read the workspace after the harness exited.
    CollectionFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SupervisorInfo {
    pub worker_pool: String,
    pub worker_id: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessRecord {
    pub name: String,
    /// Exact installed version, not the constraint.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// Harness-native session/thread id, recorded for diagnostics; never used for revisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ArtifactRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultRecord {
    pub source: ResultSource,
    pub document: ArtifactRef,
    /// Problems the supervisor found. The orchestrator re-validates regardless.
    #[serde(default)]
    pub validation_errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultSource {
    /// The agent wrote the file at `ARTIDEZI_RESULT_PATH`. Always preferred.
    File,
    /// Recovered from harness-native structured output because the file was missing.
    HarnessOutput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCollection {
    pub id: RepositoryId,
    pub base_commit: CommitSha,
    pub head_commit: CommitSha,
    pub on_configured_branch: bool,
    /// Present for read_write repositories only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<SeriesCollection>,
    /// Modified or deleted tracked files left in the working tree; any entry invalidates the attempt.
    #[serde(default)]
    pub dirty_tracked_files: Vec<String>,
    /// Untracked files are ignored, but counted so a reviewer can spot runaway output.
    #[serde(default)]
    pub untracked_files_count: u64,
    /// HEAD moved or tracked files changed in a read_only repository.
    #[serde(default)]
    pub read_only_violation: bool,
    /// Setup recipe steps run before the harness.
    #[serde(default)]
    pub setup: Vec<VerificationStep>,
    #[serde(default)]
    pub verification: Vec<VerificationStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeriesCollection {
    /// Linear, merge-free, rooted at `base_commit`, within limits.
    pub valid: bool,
    /// Limits exceeded; collection of this repository stopped.
    pub oversized: bool,
    /// Oldest first.
    pub commits: Vec<CollectedCommit>,
    /// `git bundle` of `base_commit..head_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<ArtifactRef>,
    /// `git diff base_commit head_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ArtifactRef>,
    /// Why `valid` is false, when it is.
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectedCommit {
    pub sha: CommitSha,
    pub parent: CommitSha,
    pub message: String,
    /// As written by the agent; replaced by the bot identity at publication.
    pub author: String,
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationStep {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ArtifactRef>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub timed_out: bool,
    /// Not run because an earlier step failed.
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub media_type: String,
}
