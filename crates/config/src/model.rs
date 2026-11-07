// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use artidezi_contract::common::{Outcome, RepositoryAccess};
use artidezi_contract::job::{
    Harness, HarnessSandbox, Limits, NetworkPolicy, RecipeStep, TaggingPolicy,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub instance: Instance,
    /// Keyed by endpoint name.
    pub model_endpoints: BTreeMap<String, ModelEndpoint>,
    /// Keyed by pool name.
    pub worker_pools: BTreeMap<String, WorkerPool>,
    /// Keyed by profile name.
    pub agent_profiles: BTreeMap<String, AgentProfile>,
    /// Keyed by forge name.
    pub forges: BTreeMap<String, Forge>,
    /// Keyed by project name.
    pub projects: BTreeMap<String, Project>,
    /// Optional versioned authority for named browser accounts. Current passwords drift in SQLite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_access: Option<UiAccessConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiAccessConfig {
    pub schema_version: u32,
    /// Keyed by stable user ID. Database users omitted here are disabled at UI startup.
    pub accounts: BTreeMap<String, UiAccountConfig>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiAccountConfig {
    pub display_name: String,
    /// Bootstrap verifier only. Existing database passwords are never overwritten.
    #[serde(skip_serializing)]
    pub initial_password_hash: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub instance_admin: bool,
    #[serde(default)]
    pub grants: BTreeMap<String, UiProjectGrant>,
}

impl std::fmt::Debug for UiAccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiAccountConfig")
            .field("display_name", &self.display_name)
            .field("initial_password_hash", &"[redacted]")
            .field("disabled", &self.disabled)
            .field("instance_admin", &self.instance_admin)
            .field("grants", &self.grants)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiProjectGrant {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    /// Human name of this ArtiDezi instance / trust domain.
    pub name: String,
    /// Identity used for every commit the orchestrator publishes.
    pub bot_identity: BotIdentity,
    /// Local durable directory for the SQLite database and artifact store. Never a network filesystem.
    pub data_dir: String,
    /// Opaque name of the bearer token that authenticates the admin API (task creation, revisions).
    pub admin_token_ref: String,
    /// Opt-in expiry of completed tasks' frozen repository inputs and unreferenced artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_retention: Option<ArtifactRetention>,
    /// Resource limits on worker uploads and Git bundle ingestion.
    #[serde(default)]
    pub limits: InstanceLimits,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceLimits {
    /// Largest single artifact a worker may upload.
    pub max_upload_bytes: u64,
    /// Total size of all artifacts uploaded for one attempt.
    pub max_attempt_upload_bytes: u64,
    /// Uploads received at once. Further uploads wait for a free slot.
    pub max_concurrent_uploads: u32,
    /// Worker results inspected at once per worker pool. Further results wait for a free slot.
    pub max_concurrent_inspections: u32,
    /// Largest Git bundle accepted from a worker.
    pub max_bundle_bytes: u64,
    /// Time limit for all Git work on one worker result.
    pub git_fetch_timeout_seconds: u32,
}

impl Default for InstanceLimits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 1024 * 1024 * 1024,
            max_attempt_upload_bytes: 4 * 1024 * 1024 * 1024,
            max_concurrent_uploads: 4,
            max_concurrent_inspections: 2,
            max_bundle_bytes: 512 * 1024 * 1024,
            git_fetch_timeout_seconds: 300,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRetention {
    pub completed_task_bundle_retention_seconds: u32,
    pub orphan_grace_seconds: u32,
    pub cleanup_interval_seconds: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelEndpoint {
    pub provider: ModelProvider,
    /// Provider-specific auth mode; see [`ModelProvider::auth_modes`].
    pub auth_mode: String,
    /// Opaque name resolved by the deployment secret mechanism on the worker.
    pub credential_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelProvider {
    Openai,
    Anthropic,
    /// No model at all; used by the scripted fake harness.
    None,
}

impl ModelProvider {
    pub fn auth_modes(self) -> &'static [&'static str] {
        match self {
            Self::Openai => &[
                "chatgpt_login",
                "openai_api_key",
                "openai_workload_identity",
            ],
            Self::Anthropic => &[
                "claude_subscription_token",
                "anthropic_api_key",
                "anthropic_workload_identity",
            ],
            Self::None => &["none"],
        }
    }

    pub fn serves(self, harness: Harness) -> bool {
        matches!(
            (self, harness),
            (Self::Openai, Harness::Codex)
                | (Self::Openai, Harness::Pi)
                | (Self::Anthropic, Harness::Claude)
                | (Self::None, Harness::Fake)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerPool {
    /// Opt-in reruns of lost, unreported execution. Recipes must tolerate overlapping execution.
    #[serde(default)]
    pub lost_attempt_retries: u32,
    /// Opt-in retries of explicit terminal model capacity/rate/transport failures.
    #[serde(default)]
    pub transient_failure_retries: u32,
    pub harness_sandbox: HarnessSandbox,
    /// Harnesses the pool's workers are provisioned with. The supervisor still advertises the exact
    /// installed versions; this declares intent so profiles can be validated offline.
    pub harnesses: Vec<Harness>,
    /// The workers allowed to serve this pool, by `worker_id`. This list is authoritative: a worker
    /// removed from it is rejected and forgotten.
    pub workers: BTreeMap<String, PoolWorker>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolWorker {
    /// Opaque name of the worker's pre-shared bearer token. It only permits leasing, heartbeats and
    /// uploads as this worker; assume a full-access job can steal it.
    pub token_ref: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub harness: Harness,
    pub harness_version_constraint: String,
    pub model: String,
    #[serde(default)]
    pub model_settings: BTreeMap<String, serde_json::Value>,
    /// Name of an entry in `model_endpoints`.
    pub model_endpoint: String,
    /// Name of an entry in `worker_pools`.
    pub worker_pool: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    pub provider: ForgeProvider,
    pub api_base_url: String,
    /// Browser-facing root when it differs from the controller's API/network address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_base_url: Option<String>,
    /// Opaque secret-file name: a GitHub App PEM private key, or a Forgejo token.
    pub credential_ref: String,
    /// Login of the identity ArtiDezi publishes as, for author checks and self-recognition.
    pub bot_login: String,
    /// One GitHub App installation per forge binding. GitHub currently permits dry runs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_app: Option<GithubApp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    pub client_id: String,
    pub installation_id: u64,
    /// Exact owning account of this installation, not the account that registered the App.
    pub account: String,
}

impl Config {
    /// Every configured project can receive an explicitly submitted one-off task. Publication
    /// permissions remain enforced later by the project's effects and repository access policy.
    pub fn manual_tasks_available(&self, project: &Project) -> bool {
        let _ = project;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForgeProvider {
    Github,
    Forgejo,
    /// In-process fake used by the offline test suite.
    Fake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Discovery-only: retain the assessment and patches locally, with no forge writes.
    Shadow,
    MergeRequest,
    MergeRequestWithAutomerge,
    DirectPushFastForward,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// Short human-facing explanation of the project and its repository relationships.
    pub description: String,
    /// Trusted guidance supplied to the agent for every task in this project.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_instructions: String,
    pub default_agent_profile: String,
    pub allowed_agent_profiles: Vec<String>,
    pub default_delivery_mode: DeliveryMode,
    pub allowed_delivery_modes: Vec<DeliveryMode>,
    /// Tags a worker must advertise to run this project's tasks. Until one does, tasks stay
    /// unschedulable. Tags route work only; separate trust with worker pools.
    #[serde(default)]
    pub required_worker_tags: Vec<String>,
    /// Publish agent-authored merge requests as drafts. Test configurations set this to true.
    pub draft_merge_requests: bool,
    pub network: NetworkPolicy,
    pub limits: Limits,
    pub effects: Effects,
    /// Keyed by repository id.
    pub repositories: BTreeMap<String, Repository>,
    /// Absent means no automatic discovery. Unmatched PRs are always ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<PullRequestDiscovery>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestDiscovery {
    pub poll_interval_seconds: u32,
    pub settle_seconds: u32,
    pub max_runs_per_pull_request: u32,
    pub max_active_tasks: u32,
    pub repositories: Vec<String>,
    /// Explicit project ceiling for sending selected CI log text to the model and durable store.
    #[serde(default)]
    pub ci_log_context: PullRequestCiLogContext,
    pub handlers: Vec<PullRequestHandler>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestCiLogContext {
    #[serde(default)]
    pub permitted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestHandler {
    pub id: String,
    /// Optional profile for this PR class; otherwise the project's default is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Dry run is the default; source actions require a separate explicit opt-in.
    #[serde(default)]
    pub mode: PullRequestMode,
    /// Action after an unchanged verdict and successful current forge checks.
    #[serde(default)]
    pub safe_verdict_action: SafeVerdictAction,
    /// Exact forge logins. Required unless catch_all is explicitly true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    /// Optional regular expression matched against the complete forge title after the author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
    #[serde(default)]
    pub catch_all: bool,
    pub action: PullRequestAction,
    /// Wait for every visible check to finish before assessment; an empty check set is ready
    /// after the source-head settle window. Defaults to true.
    #[serde(default = "default_wait_for_checks_to_finish")]
    pub wait_for_checks_to_finish: bool,
    pub require_successful_checks: bool,
    /// Optional point-in-time CI context frozen into the task for the agent. This does not alter
    /// intake or publication gates.
    #[serde(default, skip_serializing_if = "PullRequestCiContext::is_empty")]
    pub ci_context: PullRequestCiContext,
    pub instructions: String,
}

fn default_wait_for_checks_to_finish() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestCiContext {
    /// Include all provider-reported commit status and check-run names/states.
    #[serde(default)]
    pub results: bool,
    /// Automatically include bounded excerpts for failed GitHub Actions jobs at the source head.
    #[serde(default)]
    pub logs: bool,
}

impl PullRequestCiContext {
    fn is_empty(&self) -> bool {
        !self.results && !self.logs
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestMode {
    #[default]
    DryRun,
    Publish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestAction {
    Assess,
    Ignore,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SafeVerdictAction {
    #[default]
    Comment,
    Approve,
    /// Approve, then request an immediate merge commit, subject to forge rules.
    Merge,
}

impl SafeVerdictAction {
    pub fn is_comment(&self) -> bool {
        *self == Self::Comment
    }
}

impl PullRequestHandler {
    pub fn matches_pull_request(&self, author: &str, title: &str) -> bool {
        let author_matches = self.catch_all
            || self.authors.as_ref().is_some_and(|authors| {
                authors
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(author))
            });
        author_matches
            && self.title_regex.as_ref().is_none_or(|pattern| {
                regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(title))
            })
    }
}

/// Effects the orchestrator may perform for this project. Renovate's own automerge rules are not
/// duplicated here; these only grant what ArtiDezi itself may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Effects {
    /// Permit a merge handler to merge an untouched Renovate request after an unchanged verdict and forge checks.
    pub merge_renovate_merge_request_on_safe_verdict: bool,
    /// Publish agent-authored superseding changes (subject to the delivery mode).
    pub publish_adaptation_merge_requests: bool,
    /// Permit this project's explicit manual tasks to publish GitHub merge requests.
    ///
    /// GitHub publication remains disabled by default. Source PR assessments have their
    /// separate handler-level authority checks.
    #[serde(default)]
    pub allow_manual_github_publication: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// Name of an entry in `forges`.
    pub forge: String,
    /// Forge-specific repository path, for example `owner/name`.
    pub remote: String,
    /// Snapshot source and, for read_write repositories, the publication target.
    pub branch: String,
    pub description: String,
    pub agent_access: RepositoryAccess,
    #[serde(default)]
    pub setup_recipe: Vec<RecipeStep>,
    #[serde(default)]
    pub verification_recipe: Vec<RecipeStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagging: Option<TaggingPolicy>,
}

impl Project {
    /// Outcomes that make sense for a manual task in this project.
    pub fn manual_task_outcomes(&self) -> Vec<Outcome> {
        let mut outcomes = vec![Outcome::NeedsHuman, Outcome::Failed];
        if self
            .repositories
            .values()
            .any(|r| r.agent_access == RepositoryAccess::ReadWrite)
        {
            outcomes.insert(0, Outcome::Proposal);
        }
        outcomes
    }
}
