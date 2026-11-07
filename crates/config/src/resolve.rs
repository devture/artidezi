// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Resolution of project configuration into the immutable policy snapshot a task records.
//!
//! A task freezes the fully resolved, non-secret policy it ran under. Later configuration changes
//! never alter an existing task, and the snapshot hash makes it cheap to see which tasks ran under
//! which policy.

use std::collections::BTreeMap;

use artidezi_contract::common::{ArtifactRef, Outcome, RepositoryAccess};
use artidezi_contract::job::{
    AgentProfile as JobAgentProfile, InputRef, Limits, ModelEndpointRef, NetworkPolicy, RecipeStep,
    TaggingPolicy, WorkerRequirements,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Config, DeliveryMode, Effects, ForgeProvider};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("project {0:?} does not exist")]
    UnknownProject(String),
    #[error("agent profile {0:?} is not allowed for this project")]
    ProfileNotAllowed(String),
    #[error("delivery mode {0:?} is not allowed for this project")]
    DeliveryModeNotAllowed(DeliveryMode),
    #[error("direct_push_fast_forward requires exactly one read_write repository")]
    DirectPushNeedsSingleRepository,
}

/// Everything a task needs to know about its project, frozen at creation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProjectPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_pull_request: Option<PullRequestSource>,
    pub project: String,
    pub description: String,
    /// Trusted project guidance frozen at task creation. Absent in historical policies.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_instructions: String,
    pub delivery_mode: DeliveryMode,
    pub draft_merge_requests: bool,
    pub agent_profile_name: String,
    pub agent_profile: JobAgentProfile,
    pub worker_pool: String,
    pub worker_requirements: WorkerRequirements,
    pub network: NetworkPolicy,
    pub limits: Limits,
    pub effects: Effects,
    pub allowed_outcomes: Vec<Outcome>,
    /// Keyed by repository id.
    pub repositories: BTreeMap<String, ResolvedRepository>,
}

/// Trusted source identity and immutable input, resolved by the controller, never by the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSource {
    /// Absent on historical/dry-run tasks; records the authorized publishing identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher_login: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "crate::model::SafeVerdictAction::is_comment"
    )]
    pub safe_verdict_action: crate::model::SafeVerdictAction,
    pub tracking_id: String,
    pub repository_id: String,
    pub number: u64,
    pub url: String,
    /// Absent on historical tasks created before title-scoped routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub author: String,
    pub source_branch: String,
    pub target_branch: String,
    pub handler_id: String,
    /// Trusted handler text, rendered apart from the PR's untrusted title and description.
    /// Absent on historical tasks, whose instructions contain it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_instructions: Option<String>,
    pub input: InputRef,
    /// Point-in-time forge CI facts included only when the matched handler opted in. Names and raw
    /// provider states are untrusted data even though the controller captured them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_snapshot: Option<PullRequestCiSnapshot>,
    /// Absent on historical tasks and handlers that did not select CI logs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci_logs: Vec<PullRequestCiLog>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCiLog {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_attempt: Option<u64>,
    pub outcome: PullRequestCiLogOutcome,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub raw_bytes: u64,
    pub sanitized_bytes: u64,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<ArtifactRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestCiLogOutcome {
    Available,
    Pending,
    UnsupportedProvider,
    PermissionDenied,
    MissingOrInaccessible,
    TooLarge,
    FetchFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCiSnapshot {
    pub overall: PullRequestCiState,
    pub results: Vec<PullRequestCiResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestCiState {
    Absent,
    Pending,
    Inconclusive,
    Success,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCiResult {
    pub kind: PullRequestCiResultKind,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestCiResultKind {
    CommitStatus,
    CheckRun,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRepository {
    pub forge: String,
    pub forge_provider: ForgeProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_bot_login: Option<String>,
    pub remote: String,
    pub branch: String,
    pub description: String,
    pub access: RepositoryAccess,
    pub setup_recipe: Vec<RecipeStep>,
    pub verification_recipe: Vec<RecipeStep>,
    pub allowed_paths: Option<Vec<String>>,
    pub tagging: Option<TaggingPolicy>,
}

impl ResolvedProjectPolicy {
    /// Stable digest of the snapshot, for storage and comparison.
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("policy snapshot serializes");
        hex::encode(Sha256::digest(bytes))
    }
}

/// What an authenticated task submitter may choose. Both are structured fields, never inferred
/// from task prose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskSelection {
    pub agent_profile: Option<String>,
    pub delivery_mode: Option<DeliveryMode>,
}

impl Config {
    /// Resolves a project and the submitter's selections into a frozen policy snapshot. Assumes the
    /// configuration already passed [`crate::validate`], so cross-references exist.
    pub fn resolve_project(
        &self,
        project_name: &str,
        selection: &TaskSelection,
        allowed_outcomes: Vec<Outcome>,
    ) -> Result<ResolvedProjectPolicy, ResolveError> {
        let project = self
            .projects
            .get(project_name)
            .ok_or_else(|| ResolveError::UnknownProject(project_name.to_owned()))?;

        let profile_name = selection
            .agent_profile
            .clone()
            .unwrap_or_else(|| project.default_agent_profile.clone());
        if !project.allowed_agent_profiles.contains(&profile_name) {
            return Err(ResolveError::ProfileNotAllowed(profile_name));
        }
        let profile = &self.agent_profiles[&profile_name];
        let endpoint = &self.model_endpoints[&profile.model_endpoint];
        let pool = &self.worker_pools[&profile.worker_pool];

        let delivery_mode = selection
            .delivery_mode
            .unwrap_or(project.default_delivery_mode);
        if !project.allowed_delivery_modes.contains(&delivery_mode) {
            return Err(ResolveError::DeliveryModeNotAllowed(delivery_mode));
        }

        let repositories: BTreeMap<String, ResolvedRepository> = project
            .repositories
            .iter()
            .map(|(id, repo)| {
                let forge = &self.forges[&repo.forge];
                (
                    id.clone(),
                    ResolvedRepository {
                        forge: repo.forge.clone(),
                        forge_provider: forge.provider,
                        forge_api_base_url: Some(forge.api_base_url.clone()),
                        forge_bot_login: Some(forge.bot_login.clone()),
                        remote: repo.remote.clone(),
                        branch: repo.branch.clone(),
                        description: repo.description.clone(),
                        access: repo.agent_access,
                        setup_recipe: repo.setup_recipe.clone(),
                        verification_recipe: repo.verification_recipe.clone(),
                        allowed_paths: repo.allowed_paths.clone(),
                        tagging: repo.tagging.clone(),
                    },
                )
            })
            .collect();

        if delivery_mode == DeliveryMode::DirectPushFastForward
            && repositories
                .values()
                .filter(|r| r.access == RepositoryAccess::ReadWrite)
                .count()
                != 1
        {
            return Err(ResolveError::DirectPushNeedsSingleRepository);
        }

        Ok(ResolvedProjectPolicy {
            source_pull_request: None,
            project: project_name.to_owned(),
            description: project.description.clone(),
            agent_instructions: project.agent_instructions.clone(),
            delivery_mode,
            draft_merge_requests: project.draft_merge_requests,
            agent_profile_name: profile_name.clone(),
            agent_profile: JobAgentProfile {
                name: profile_name,
                harness: profile.harness,
                harness_version_constraint: profile.harness_version_constraint.clone(),
                model: profile.model.clone(),
                model_settings: profile.model_settings.clone(),
                model_endpoint: ModelEndpointRef {
                    auth_mode: endpoint.auth_mode.clone(),
                    credential_ref: endpoint.credential_ref.clone(),
                    base_url: endpoint.base_url.clone(),
                },
            },
            worker_pool: profile.worker_pool.clone(),
            worker_requirements: WorkerRequirements {
                harness_sandbox: pool.harness_sandbox,
                required_tags: project.required_worker_tags.clone(),
            },
            network: project.network.clone(),
            limits: project.limits,
            effects: project.effects,
            allowed_outcomes,
            repositories,
        })
    }
}
