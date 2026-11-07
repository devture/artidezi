// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! `result.json`: the agent's structured output. Every field is untrusted.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{CommitSha, Outcome, RepositoryId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentResult {
    pub schema_version: u32,
    pub outcome: Outcome,
    /// Markdown for the merge-request body and dashboard. Rendered as untrusted text.
    pub summary: String,
    /// The agent's own rating, recorded as context. It's not independent proof and gates nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_hint: Option<RiskHint>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// One entry per read_write repository in the job, in any order.
    pub repositories: Vec<ResultRepository>,
    /// Required non-empty for `needs_human`, empty otherwise.
    #[serde(default)]
    pub questions_for_human: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_for_next_attempt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RiskHint {
    pub level: RiskLevel,
    pub rationale: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub kind: EvidenceKind,
    /// URL or short description; untrusted.
    pub source: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    ReleaseNotes,
    Changelog,
    UpstreamDiff,
    Documentation,
    LocalCheck,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultRepository {
    pub id: RepositoryId,
    pub changed: bool,
    /// The HEAD the agent believes it left; must equal the collected HEAD when `changed` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<CommitSha>,
    /// Allowed only when the job's tagging policy permits proposals; must match its pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_proposal: Option<String>,
}
