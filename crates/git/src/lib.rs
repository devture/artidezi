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
pub mod conflicts;
pub mod equivalence;
pub mod publish;
pub mod series;
pub mod workspace;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    /// A remote operation failed for a reason that another try later can fix.
    #[error("temporary git network error: {0}")]
    Temporary(git2::Error),
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

/// Sorts an error from a remote operation. Only lost connections, timeouts and server-side HTTP
/// failures are temporary: refused leases, credentials, certificates, unknown repositories and
/// protocol violations stay what they are.
pub(crate) fn remote_error(error: git2::Error) -> GitError {
    if is_temporary(error.code(), error.class(), error.message()) {
        GitError::Temporary(error)
    } else {
        GitError::Git(error)
    }
}

fn is_temporary(code: git2::ErrorCode, class: git2::ErrorClass, message: &str) -> bool {
    use git2::{ErrorClass, ErrorCode};
    if matches!(
        code,
        ErrorCode::Auth | ErrorCode::Certificate | ErrorCode::NotFound | ErrorCode::User
    ) {
        return false;
    }
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| message.starts_with(prefix));
    match class {
        // libgit2 also uses this class for protocol violations and errors the server reports.
        ErrorClass::Net => starts(&[
            "failed to connect to ",
            "failed to resolve address for ",
            "error receiving data from socket",
            "error sending data to socket",
            "could not read from remote repository",
            "could not read refs from remote repository",
            "could not read report from remote repository",
            "SSL error:",
        ]),
        ErrorClass::Os => starts(&["failed to connect to ", "SSL error: syscall failure"]),
        ErrorClass::Ssl => starts(&[
            "SSL error: connection failure",
            "SSL error: received early EOF",
            "SSL error: unknown error",
        ]),
        ErrorClass::Http => {
            message == "unexpected EOF"
                || message
                    .strip_prefix("unexpected http status code: ")
                    .and_then(|status| status.trim().parse::<u16>().ok())
                    .is_some_and(|status| matches!(status, 408 | 429 | 500..=599))
        }
        _ => false,
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> GitError {
    let path = path.into();
    move |source| GitError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connection_and_server_failures_are_temporary() {
        use git2::{ErrorClass, ErrorCode};
        let generic = ErrorCode::GenericError;
        for (class, message) in [
            (ErrorClass::Os, "failed to connect to forge.invalid"),
            (
                ErrorClass::Net,
                "failed to connect to forge.invalid: Operation timed out",
            ),
            (
                ErrorClass::Net,
                "failed to resolve address for forge.invalid: Temporary failure in name resolution",
            ),
            (
                ErrorClass::Net,
                "error receiving data from socket: Connection reset by peer",
            ),
            (ErrorClass::Ssl, "SSL error: received early EOF"),
            (ErrorClass::Http, "unexpected http status code: 503"),
            (ErrorClass::Http, "unexpected http status code: 429"),
        ] {
            assert!(is_temporary(generic, class, message), "{message}");
        }
        for (code, class, message) in [
            (
                generic,
                ErrorClass::Http,
                "unexpected http status code: 403",
            ),
            (
                generic,
                ErrorClass::Http,
                "too many redirects or authentication replays",
            ),
            (
                ErrorCode::Auth,
                ErrorClass::Http,
                "remote authentication required but no callback set",
            ),
            (
                ErrorCode::Certificate,
                ErrorClass::Ssl,
                "the SSL certificate is invalid",
            ),
            (generic, ErrorClass::Net, "remote error: access denied"),
            (
                generic,
                ErrorClass::None,
                "push lease failed: expected a, found b",
            ),
            (generic, ErrorClass::Os, "could not write: No space left"),
            (
                generic,
                ErrorClass::Reference,
                "cannot push non-fastforwardable reference",
            ),
        ] {
            assert!(!is_temporary(code, class, message), "{message}");
        }
    }

    #[test]
    fn an_unreachable_remote_is_a_temporary_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache::BareCache::open_or_init(&dir.path().join("cache.git")).unwrap();
        // Bind and drop a port so that nothing listens on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = cache
            .fetch(
                &format!("http://127.0.0.1:{port}/org/app.git"),
                &["+refs/heads/main:refs/heads/main"],
                None,
            )
            .unwrap_err();
        assert!(matches!(error, GitError::Temporary(_)), "{error}");
    }

    #[test]
    fn packaged_git_supports_https_remotes() {
        // git2 0.21 has no default transport features. Vendoring OpenSSL alone does not
        // enable HTTPS; the first private Forgejo deployment caught that omission.
        assert!(git2::Version::get().https());
    }
}
