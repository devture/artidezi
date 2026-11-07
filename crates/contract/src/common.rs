// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of every contract document defined in this crate.
pub const SCHEMA_VERSION: u32 = 1;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
    /// Identifies one agent invocation. Immutable once created.
    AttemptId
);
string_id!(
    /// Identifies a durable task; a task owns many attempts.
    TaskId
);
string_id!(
    /// Stable repository identifier declared in project configuration.
    RepositoryId
);

/// A full 40-character lowercase hexadecimal Git object id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct CommitSha(pub String);

impl CommitSha {
    pub fn is_well_formed(&self) -> bool {
        self.0.len() == 40
            && self
                .0
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for CommitSha {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Reference to an immutable artifact in the content-addressed store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRef {
    /// Lowercase hexadecimal SHA-256 of the artifact bytes.
    pub sha256: String,
    /// Size in bytes; lets receivers refuse oversized downloads before fetching.
    pub size: u64,
}

impl ArtifactRef {
    pub fn is_well_formed(&self) -> bool {
        self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }
}

/// Whether the agent may modify a repository. `read_only` repositories exist for context only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccess {
    ReadOnly,
    ReadWrite,
}

/// The agent's declared outcome for an attempt. Which outcomes are allowed is decided per job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The untouched input change (typically a Renovate merge request) is fine as-is.
    ApproveUntouched,
    /// The agent prepared a commit series in at least one read_write repository.
    Proposal,
    /// The agent could not decide or act and has questions for a human.
    NeedsHuman,
    /// The agent could not complete the work, for example because of an environment problem.
    Failed,
}
