// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The worker supervisor. It runs inside the (untrusted-after-compromise) worker VM, so it holds no
//! forge credentials and its uploads are re-validated by the orchestrator. Its job is mechanical:
//! lay out a fresh workspace, run the harness, collect what the agent left behind, run the
//! configured verification recipe and package everything into an attempt record.

pub mod attempt;
pub mod client;
mod credentials;
pub mod daemon;
pub mod process;

pub use attempt::{
    AttemptInputs, AttemptOutput, PreviousInputs, StagedArtifact, WorkerConfig, run_attempt,
    run_attempt_cancellable,
};
pub use daemon::{Daemon, DaemonConfig};
pub use process::Cancellation;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("attempt cancelled or lease revoked")]
    Cancelled,
    #[error("process group {pid} ({program}) did not exit after SIGKILL; worker recovery required")]
    UnstoppableProcess { program: PathBuf, pid: u32 },
    #[error(
        "worker quarantined at {path}; recover the worker environment before removing this marker"
    )]
    Quarantined { path: PathBuf },
    #[error("{operation}: HTTP {status}: {message}")]
    Http {
        operation: String,
        status: u16,
        message: String,
    },
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git error: {0}")]
    Git(#[from] artidezi_git::GitError),
    #[error("libgit2 error: {0}")]
    Git2(#[from] git2::Error),
    #[error("harness error: {0}")]
    Harness(#[from] artidezi_harness::HarnessError),
    #[error("{0}")]
    Unreachable(String),
    #[error("{0}")]
    Invalid(String),
}

impl WorkerError {
    /// The orchestrator or its proxy is away, as during a restart. A proxy which routes by
    /// container labels has no route while the container is gone, so it answers 404.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Unreachable(_)
                | Self::Http {
                    status: 404 | 502..=504,
                    ..
                }
        )
    }
}

pub type Result<T> = std::result::Result<T, WorkerError>;

pub(crate) fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> WorkerError {
    let path = path.into();
    move |source| WorkerError::Io { path, source }
}
