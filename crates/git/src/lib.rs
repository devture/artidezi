// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Git plumbing shared by the orchestrator and the worker supervisor.
//!
//! Repository operations use libgit2 through `git2`. Bundles are the one exception: libgit2 has no
//! bundle support, so [`bundle`] runs the `git` binary with an argument vector (never a shell) for
//! `bundle create`, `bundle verify`, `fetch <bundle>` and `clone <bundle>`.
//!
//! Nothing here decides policy. Callers pass exact object ids and receive facts.

pub mod bundle;
pub mod cache;
pub mod equivalence;
pub mod publish;
pub mod series;
pub mod workspace;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git command {command:?} failed with {status}: {stderr}")]
    Command {
        command: Vec<String>,
        status: String,
        stderr: String,
    },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// Author and committer identity for commits the orchestrator creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    pub fn signature(&self) -> Result<git2::Signature<'static>> {
        Ok(git2::Signature::now(&self.name, &self.email)?)
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> GitError {
    let path = path.into();
    move |source| GitError::Io { path, source }
}

#[cfg(test)]
mod tests {
    #[test]
    fn packaged_git_supports_https_remotes() {
        // git2 0.21 has no default transport features. Vendoring OpenSSL alone does not
        // enable HTTPS; the first private Forgejo deployment caught that omission.
        assert!(git2::Version::get().https());
    }
}
