// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Domain records as stored. State machines are enforced by [`crate::Db`] on every transition.

use artidezi_config::model::DeliveryMode;
use artidezi_contract::common::{ArtifactRef, Outcome};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Free-form instructions posted by an authenticated human.
    Manual,
    /// Evaluation of an existing bot merge request discovered by polling.
    Renovate,
    /// Assessment of a PR selected by a configured discovery handler.
    PullRequest,
}

/// Persistent source tracking. Human holds and budgets survive new tasks and restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackedPullRequest {
    pub id: String,
    pub project: String,
    pub repository_id: String,
    pub forge: String,
    pub remote: String,
    pub number: u64,
    pub url: String,
    /// Latest observed source title and closure state; older records may omit them.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub closed: bool,
    pub handler_id: String,
    pub observed_head: String,
    pub head_observed_at: DateTime<Utc>,
    pub last_task_id: Option<String>,
    pub runs: u32,
    pub paused: bool,
    pub status: String,
    /// A known unresolved source can be revisited without listing its repository again.
    #[serde(default)]
    pub follow_up: Option<PullRequestFollowUp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PullRequestFollowUp {
    pub at: DateTime<Utc>,
    pub reason: PullRequestFollowUpReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestFollowUpReason {
    Settling,
    Checks,
    Draft,
    Capacity,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// An attempt is queued or running.
    Working,
    /// A validated unchanged source assessment is waiting for forge CI before approval/merge.
    WaitingForCi,
    /// The latest attempt produced a proposal that is published and awaits a human or automerge.
    AwaitingReview,
    /// The latest attempt asked for human input, or validation failed and a human must decide.
    NeedsHuman,
    /// Delivered: merged, pushed or approved. Terminal.
    Done,
    /// Closed by a human without delivery. Terminal.
    Closed,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Closed)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            (
                Working,
                WaitingForCi | AwaitingReview | NeedsHuman | Done | Closed
            ) | (WaitingForCi, Working | NeedsHuman | Done | Closed)
                | (AwaitingReview, Working | NeedsHuman | Done | Closed)
                | (NeedsHuman, Working | Closed)
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::WaitingForCi => "waiting_for_ci",
            Self::AwaitingReview => "awaiting_review",
            Self::NeedsHuman => "needs_human",
            Self::Done => "done",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    /// Waiting for a worker of the right pool.
    Queued,
    /// Leased by a worker; heartbeats extend the lease.
    Leased,
    /// The worker uploaded an attempt record; the orchestrator has not judged it yet.
    Uploaded,
    /// Validated: the result and any commit series passed every orchestrator check.
    Valid,
    /// Validation failed; details in `validation_problems`.
    Invalid,
    /// The lease expired without an upload.
    Lost,
    /// Cancelled before completion.
    Cancelled,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Valid | Self::Invalid | Self::Lost | Self::Cancelled
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use AttemptState::*;
        matches!(
            (self, next),
            (Queued, Leased | Cancelled)
                | (Leased, Uploaded | Lost | Cancelled)
                | (Uploaded, Valid | Invalid)
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Uploaded => "uploaded",
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Lost => "lost",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Stable instance-wide human reference, assigned on insertion. Zero before insertion.
    #[serde(default)]
    pub number: i64,
    pub project: String,
    pub kind: TaskKind,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Authenticated actor that created the task (a UI user or the poller).
    pub created_by: String,
    /// Untrusted free-form instructions or the discovered merge-request description.
    pub instructions: String,
    pub delivery_mode: DeliveryMode,
    pub agent_profile: String,
    /// Frozen [`artidezi_config::ResolvedProjectPolicy`] as JSON.
    pub policy_snapshot: String,
    pub policy_digest: String,
    pub state: TaskState,
    pub repositories: Vec<TaskRepository>,
}

/// The exact base a task works from. Frozen at creation; revisions record a new base per attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRepository {
    pub repository_id: String,
    pub branch: String,
    pub base_commit: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub task_id: String,
    pub number: u32,
    pub state: AttemptState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub worker_pool: String,
    pub job: ArtifactRef,
    /// Authenticated human feedback that triggered this revision, if any.
    pub human_feedback: Option<String>,
    /// Controller backoff; independent of the immutable job and a worker's lease.
    #[serde(default)]
    pub not_before: Option<DateTime<Utc>>,
    pub attempt_record: Option<ArtifactRef>,
    pub result: Option<ArtifactRef>,
    pub outcome: Option<Outcome>,
    pub validation_problems: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub attempt_id: String,
    pub worker_pool: String,
    pub worker_id: String,
    pub leased_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Supervisor observations and operator scheduling controls, not independent attestation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    pub worker_pool: String,
    pub worker_id: String,
    pub last_seen: DateTime<Utc>,
    pub quarantined: bool,
    pub drained: bool,
    pub harnesses: Vec<artidezi_contract::wire::AdvertisedHarness>,
    pub tags: Vec<String>,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub attempt_id: Option<String>,
    pub task_number: Option<i64>,
    pub lease_expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeRequestState {
    Open,
    Merged,
    Closed,
}

impl MergeRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

/// A merge request ArtiDezi published for a task. One per read_write repository at most.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedMergeRequest {
    pub task_id: String,
    pub repository_id: String,
    pub forge: String,
    pub number: u64,
    pub url: String,
    pub source_branch: String,
    /// Head of the source branch as ArtiDezi last wrote it; the lease for rewrites.
    pub head_commit: String,
    pub state: MergeRequestState,
    pub updated_at: DateTime<Utc>,
}

/// Append-only audit event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub task_id: Option<String>,
    pub attempt_id: Option<String>,
    pub actor: String,
    pub kind: String,
    pub payload: serde_json::Value,
}
