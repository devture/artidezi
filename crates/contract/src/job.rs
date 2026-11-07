// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! `job.json`: what the orchestrator hands the worker supervisor for one attempt.
//!
//! Deliberately absent: the delivery mode (the agent must not know or influence how its output is
//! delivered) and any secret value (`credential_ref` names a credential the worker already holds).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{
    ArtifactRef, AttemptId, CommitSha, Outcome, RepositoryAccess, RepositoryId, TaskId,
};

/// Version 2 permits revision jobs without previous artifacts after an unreported attempt.
/// Existing version 1 jobs remain readable and retain their original validation rules.
pub const JOB_SCHEMA_VERSION: u32 = 2;
pub const SUPPORTED_JOB_SCHEMA_VERSIONS: &[u32] = &[1, JOB_SCHEMA_VERSION];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub schema_version: u32,
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    /// 1 for the first attempt of a task; revisions increment it.
    pub attempt_number: u32,
    /// Version of the template that rendered `task.md`, so prompt changes are auditable.
    pub prompt_template_version: u32,
    pub agent_profile: AgentProfile,
    pub worker_requirements: WorkerRequirements,
    pub network: NetworkPolicy,
    pub limits: Limits,
    /// Outcomes that make sense for this task; the result must use one of them.
    pub allowed_outcomes: Vec<Outcome>,
    pub repositories: Vec<JobRepository>,
    /// The rendered `task.md`.
    pub task_document: ArtifactRef,
    /// Previous artifacts for a revision (attempt_number > 1). Required in version 1;
    /// in version 2 absent when the last attempt returned no execution record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_attempt: Option<PreviousAttempt>,
}

/// The fully resolved, non-secret agent profile used for this attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub name: String,
    pub harness: Harness,
    /// Cargo-style version requirement the worker's installed harness must satisfy.
    pub harness_version_constraint: String,
    /// Harness model name; `default` lets the harness choose its own default.
    pub model: String,
    /// Harness-specific model settings, passed through opaquely as the harness's own config keys
    /// (for Codex: `model_reasoning_effort`).
    #[serde(default)]
    pub model_settings: BTreeMap<String, serde_json::Value>,
    pub model_endpoint: ModelEndpointRef,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Codex,
    Pi,
    Claude,
    /// Scripted harness used by the offline test suite.
    Fake,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelEndpointRef {
    /// Provider-specific auth mode name, for example `chatgpt_login` or `openai_api_key`.
    pub auth_mode: String,
    /// Opaque name of a credential the worker holds. Never the credential itself.
    pub credential_ref: String,
    /// Base URL override for API-style endpoints; absent means the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// What a worker must offer to run a job. Unknown fields are ignored, because stored jobs may
/// carry a `container_runtime` that no longer has any meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerRequirements {
    pub harness_sandbox: HarnessSandbox,
    /// Tags the worker must advertise. These route work and are not a trust boundary:
    /// workers report their own tags, so only a worker pool separates trust.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tags: Vec<String>,
}

impl WorkerRequirements {
    pub fn met_by(&self, tags: &[String]) -> bool {
        self.required_tags.iter().all(|tag| tags.contains(tag))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSandbox {
    Strict,
    Disabled,
}

/// Per-attempt network policy. Command egress and web search are controlled separately because
/// harnesses expose them separately; a task may narrow the project policy, never broaden it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    pub command_egress: CommandEgress,
    pub web_search: WebSearch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandEgress {
    pub mode: EgressMode,
    /// Mandatory when `mode` is `domain_allowlist`, forbidden otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub denied_domains: Vec<String>,
    pub private_network: PrivateNetworkMode,
    #[serde(default)]
    pub allowed_private_endpoints: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    Blocked,
    DomainAllowlist,
    /// Arbitrary public upstreams; never implies LAN or private-endpoint access.
    PublicInternet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrivateNetworkMode {
    Blocked,
    DomainAllowlist,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearch {
    pub mode: WebSearchMode,
    pub domain_scope: WebSearchScope,
    /// Mandatory when `domain_scope` is `domain_allowlist`, forbidden otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchMode {
    Disabled,
    Cached,
    Indexed,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchScope {
    PublicInternet,
    DomainAllowlist,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub wall_clock_seconds: u64,
    pub result_json_max_bytes: u64,
    pub max_commits_per_repository: u32,
    pub max_diff_bytes_per_repository: u64,
    pub max_commit_message_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobRepository {
    pub id: RepositoryId,
    pub access: RepositoryAccess,
    /// The configured branch; the checkout sits on a local branch of this name at `base_commit`.
    pub branch: String,
    pub base_commit: CommitSha,
    /// Git bundle containing `base_commit` and its history; no remotes.
    pub snapshot_bundle: ArtifactRef,
    /// Extra read-only commits, exposed as `refs/artidezi/inputs/<name>`.
    #[serde(default)]
    pub input_refs: Vec<InputRef>,
    #[serde(default)]
    pub setup_recipe: Vec<RecipeStep>,
    #[serde(default)]
    pub verification_recipe: Vec<RecipeStep>,
    /// Glob patterns the series may touch. Enforced by the orchestrator, never by the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagging: Option<TaggingPolicy>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputRef {
    pub name: String,
    pub commit: CommitSha,
    /// Bundle providing the commit's objects on top of the snapshot.
    pub bundle: ArtifactRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeStep {
    pub name: String,
    /// argv, executed without a shell.
    pub command: Vec<String>,
    pub timeout_seconds: u64,
    /// Relative to the repository root.
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

fn default_cwd() -> String {
    ".".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaggingPolicy {
    pub agent_may_propose: bool,
    /// Anchored regular expression a proposed tag must match.
    pub allowed_pattern: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviousAttempt {
    pub attempt_number: u32,
    pub attempt_document: ArtifactRef,
    /// Absent when the previous attempt produced no parseable result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_document: Option<ArtifactRef>,
    #[serde(default)]
    pub commit_bundles: BTreeMap<RepositoryId, ArtifactRef>,
}

impl Job {
    pub fn repository(&self, id: &RepositoryId) -> Option<&JobRepository> {
        self.repositories.iter().find(|r| &r.id == id)
    }

    pub fn read_write_repositories(&self) -> impl Iterator<Item = &JobRepository> {
        self.repositories
            .iter()
            .filter(|r| r.access == RepositoryAccess::ReadWrite)
    }
}

impl AgentProfile {
    /// Whether one of the harnesses a worker advertises can run this profile.
    pub fn runs_on(&self, harnesses: &[crate::wire::AdvertisedHarness]) -> bool {
        let name = format!("{:?}", self.harness).to_lowercase();
        harnesses.iter().any(|harness| {
            harness.name == name
                && version_satisfies(&self.harness_version_constraint, &harness.version)
        })
    }
}

/// Harnesses report versions in their own format, such as `codex-cli 0.163.0`.
fn version_satisfies(constraint: &str, version: &str) -> bool {
    if constraint.trim() == "*" {
        return true;
    }
    let Ok(requirement) = semver::VersionReq::parse(constraint) else {
        return false;
    };
    version
        .split_whitespace()
        .filter_map(|word| semver::Version::parse(word.trim_start_matches('v')).ok())
        .any(|version| requirement.matches(&version))
}

#[cfg(test)]
mod version_tests {
    use super::version_satisfies;

    #[test]
    fn constraints_match_the_version_inside_harness_output() {
        assert!(version_satisfies("=0.163.0", "codex-cli 0.163.0"));
        assert!(!version_satisfies("=0.163.0", "codex-cli 0.164.0"));
        assert!(version_satisfies("*", "fake-1"));
        assert!(!version_satisfies("=1.0.0", "fake-1"));
    }
}
