// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Harness adapters translate a [`Job`] into a concrete process invocation and read harness-native
//! facts (session id, model, usage) back out of its output. Adapters never decide policy and never
//! see forge credentials.

pub mod codex;
pub mod fake;
pub mod pi;
mod version;

use std::path::{Path, PathBuf};

use artidezi_contract::Job;
use artidezi_contract::job::Harness;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness version probe could not be killed: {program} (pid {pid})")]
    UnstoppableProcess { program: PathBuf, pid: u32 },
    #[error("harness {0:?} is not supported by this adapter")]
    Unsupported(Harness),
    #[error("missing credential material for {credential_ref}: {reason}")]
    Credential {
        credential_ref: String,
        reason: String,
    },
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, HarnessError>;

/// Everything an adapter may use to build the invocation. Paths are absolute.
pub struct InvocationContext<'a> {
    pub job: &'a Job,
    /// `$ARTIDEZI_WORKSPACE`
    pub workspace: &'a Path,
    /// `task/task.md`
    pub task_document: &'a Path,
    /// `out/result.json`, `$ARTIDEZI_RESULT_PATH`
    pub result_path: &'a Path,
    /// Directory of the first read_write repository (or the first repository when none is writable).
    pub primary_repo_dir: &'a Path,
    /// Fresh per-attempt HOME.
    pub home: &'a Path,
    /// Fresh per-attempt scratch directory, `$TMPDIR`. A sibling of `home`, never its parent:
    /// Codex refuses to create helper binaries in a `CODEX_HOME` under the temporary directory.
    pub tmp: &'a Path,
    /// Directory holding the credential material named by the job's `credential_ref`, if the
    /// supervisor has it. Adapters copy what they need into the attempt's fresh state.
    pub credential_dir: Option<&'a Path>,
}

/// A process to run. Executed by the supervisor with the workspace's network policy in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub stdin: Option<Vec<u8>>,
    /// Attempt-local credential files that the harness may rotate and the worker should persist
    /// back to its durable credential root after the process exits.
    pub credential_updates: Vec<CredentialFileUpdate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialFileKind {
    CodexChatgptAuth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialFileUpdate {
    pub credential_ref: String,
    pub source: PathBuf,
    pub attempt: PathBuf,
    pub kind: CredentialFileKind,
}

/// Facts an adapter can recover from harness output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptFacts {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// A result document recovered from harness-native structured output, used only when the
    /// agent did not write the result file.
    pub fallback_result: Option<serde_json::Value>,
}

pub trait HarnessAdapter: Send + Sync {
    fn harness(&self) -> Harness;
    /// Exact installed version, recorded with every attempt.
    fn installed_version(&self) -> Result<String>;
    fn build_invocation(&self, ctx: &InvocationContext<'_>) -> Result<Invocation>;
    fn read_transcript(&self, stdout: &[u8]) -> TranscriptFacts;
}

/// Environment every harness gets, regardless of adapter.
pub fn common_env(ctx: &InvocationContext<'_>) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), ctx.home.display().to_string()),
        (
            "XDG_CONFIG_HOME".into(),
            ctx.home.join(".config").display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".into(),
            ctx.home.join(".cache").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".into(),
            ctx.home.join(".local/share").display().to_string(),
        ),
        ("TMPDIR".into(), ctx.tmp.display().to_string()),
        (
            "ARTIDEZI_WORKSPACE".into(),
            ctx.workspace.display().to_string(),
        ),
        (
            "ARTIDEZI_TASK_DOCUMENT".into(),
            ctx.task_document.display().to_string(),
        ),
        (
            "ARTIDEZI_RESULT_PATH".into(),
            ctx.result_path.display().to_string(),
        ),
        ("ARTIDEZI_ATTEMPT_ID".into(), ctx.job.attempt_id.to_string()),
        ("GIT_AUTHOR_NAME".into(), "ArtiDezi worker".into()),
        ("GIT_AUTHOR_EMAIL".into(), "worker@artidezi.invalid".into()),
        ("GIT_COMMITTER_NAME".into(), "ArtiDezi worker".into()),
        (
            "GIT_COMMITTER_EMAIL".into(),
            "worker@artidezi.invalid".into(),
        ),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
    ]
}
