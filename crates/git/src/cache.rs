// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The orchestrator's trusted bare cache per repository. It is rebuildable acceleration: the forge
//! remains authoritative and every publication re-resolves the target there.

use std::path::{Path, PathBuf};

use git2::{FetchOptions, Oid, PushOptions, RemoteCallbacks, Repository};

use crate::{GitError, Result};

/// Credentials for HTTPS remotes. Absent for local-path remotes used in tests.
#[derive(Clone)]
pub struct HttpsCredentials {
    pub username: String,
    pub token: String,
}

impl std::fmt::Debug for HttpsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsCredentials")
            .field("username", &self.username)
            .field("token", &"[redacted]")
            .finish()
    }
}

pub struct BareCache {
    repo: Repository,
    path: PathBuf,
}

impl BareCache {
    pub fn open_or_init(path: &Path) -> Result<Self> {
        let repo = match Repository::open_bare(path) {
            Ok(repo) => repo,
            Err(_) => {
                std::fs::create_dir_all(path).map_err(crate::io_err(path))?;
                Repository::init_bare(path)?
            }
        };
        Ok(Self {
            repo,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    fn callbacks<'a>(
        url: &'a str,
        credentials: Option<&'a HttpsCredentials>,
    ) -> RemoteCallbacks<'a> {
        let mut callbacks = RemoteCallbacks::new();
        if let Some(creds) = credentials {
            callbacks.credentials(move |requested_url, _username, _allowed| {
                if requested_url.trim_end_matches('/') != url.trim_end_matches('/') {
                    return Err(git2::Error::from_str(
                        "refusing Git credentials for a different URL",
                    ));
                }
                git2::Cred::userpass_plaintext(&creds.username, &creds.token)
            });
        }
        callbacks
    }

    /// Fetches `refspecs` from `url` using an anonymous remote, so no remote configuration is
    /// persisted in the cache.
    pub fn fetch(
        &self,
        url: &str,
        refspecs: &[&str],
        credentials: Option<&HttpsCredentials>,
    ) -> Result<()> {
        let mut remote = self.repo.remote_anonymous(url)?;
        let mut options = FetchOptions::new();
        options.remote_callbacks(Self::callbacks(url, credentials));
        options.follow_redirects(git2::RemoteRedirect::None);
        options.download_tags(git2::AutotagOption::None);
        options.prune(git2::FetchPrune::Off);
        remote.fetch(refspecs, Some(&mut options), None)?;
        Ok(())
    }

    /// Pushes one refspec. Rewrites and deletions require an explicit lease.
    pub fn push(
        &self,
        url: &str,
        refspec: &str,
        credentials: Option<&HttpsCredentials>,
    ) -> Result<()> {
        if refspec.starts_with('+') || refspec.starts_with(':') {
            return Err(GitError::Invalid(
                "forced push or deletion requires an explicit lease".into(),
            ));
        }
        self.push_inner(url, refspec, credentials, None)
    }

    /// Pushes only if the remote advertises exactly `expected_head` during push negotiation.
    /// Use `Oid::ZERO_SHA1` to require creation of a previously absent branch. The receive-pack
    /// update also carries this old OID, so a later concurrent update is rejected by the forge.
    /// A deletion of an already absent ref succeeds, allowing retries with the original lease.
    pub fn push_with_lease(
        &self,
        url: &str,
        refspec: &str,
        credentials: Option<&HttpsCredentials>,
        expected_head: Oid,
    ) -> Result<()> {
        self.push_inner(url, refspec, credentials, Some(expected_head))
    }

    fn push_inner(
        &self,
        url: &str,
        refspec: &str,
        credentials: Option<&HttpsCredentials>,
        expected_head: Option<Oid>,
    ) -> Result<()> {
        let rejected = std::cell::RefCell::new(None::<String>);
        {
            let mut remote = self.repo.remote_anonymous(url)?;
            let mut callbacks = Self::callbacks(url, credentials);
            if let Some(expected) = expected_head {
                callbacks.push_negotiation(move |updates| {
                    let [update] = updates else {
                        return Err(git2::Error::from_str(
                            "lease requires exactly one ref update",
                        ));
                    };
                    let absent_deletion = refspec.starts_with(':') && update.src().is_zero();
                    if update.src() != expected && !absent_deletion {
                        return Err(git2::Error::from_str(&format!(
                            "push lease failed: expected {expected}, found {}",
                            update.src()
                        )));
                    }
                    Ok(())
                });
            }
            callbacks.push_update_reference(|refname, status| {
                if let Some(status) = status {
                    *rejected.borrow_mut() = Some(format!("{refname}: {status}"));
                }
                Ok(())
            });
            let mut options = PushOptions::new();
            options.remote_callbacks(callbacks);
            options.follow_redirects(git2::RemoteRedirect::None);
            remote.push(&[refspec], Some(&mut options))?;
        }
        match rejected.into_inner() {
            Some(reason) => Err(GitError::Invalid(format!("push rejected: {reason}"))),
            None => Ok(()),
        }
    }

    pub fn resolve(&self, refname: &str) -> Result<Oid> {
        Ok(self.repo.refname_to_id(refname)?)
    }

    pub fn set_ref(&self, refname: &str, oid: Oid, log_message: &str) -> Result<()> {
        self.repo.reference(refname, oid, true, log_message)?;
        Ok(())
    }

    pub fn delete_ref(&self, refname: &str) -> Result<()> {
        if let Ok(mut reference) = self.repo.find_reference(refname) {
            reference.delete()?;
        }
        Ok(())
    }

    pub fn has_commit(&self, oid: Oid) -> bool {
        self.repo.find_commit(oid).is_ok()
    }
}
