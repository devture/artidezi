// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Preparation of the credential-free checkout an agent works in.

use std::path::Path;

use git2::{Oid, Repository};

use crate::{GitError, Result, bundle};

pub struct InputRefSpec<'a> {
    pub name: &'a str,
    pub commit: Oid,
    pub bundle_path: &'a Path,
}

/// Clones the snapshot bundle into `dest`, creates a local branch `branch` at `base_commit`, checks
/// it out and adds every input ref under `refs/artidezi/inputs/<name>`. The result has no remotes.
pub fn prepare_checkout(
    snapshot_bundle: &Path,
    dest: &Path,
    branch: &str,
    base_commit: Oid,
    input_refs: &[InputRefSpec<'_>],
) -> Result<Repository> {
    bundle::clone_no_checkout(snapshot_bundle, dest)?;
    let repo = Repository::open(dest)?;
    repo.find_commit(base_commit).map_err(|_| {
        GitError::Invalid(format!(
            "snapshot bundle does not contain base commit {base_commit}"
        ))
    })?;
    let branch_ref = format!("refs/heads/{branch}");
    // A no-checkout clone still has its default branch checked out in Git's metadata. Updating
    // that existing branch with `repo.branch(..., true)` therefore fails with "cannot force
    // update ... as it is the current HEAD". Update an existing ref directly instead.
    match repo.find_reference(&branch_ref) {
        Ok(mut reference) => {
            reference.set_target(base_commit, "Reset ArtiDezi work branch")?;
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            let commit = repo.find_commit(base_commit)?;
            repo.branch(branch, &commit, false)?;
        }
        Err(error) => return Err(error.into()),
    }
    repo.set_head(&branch_ref)?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
    for input in input_refs {
        let refname = format!("refs/artidezi/inputs/{}", input.name);
        bundle::fetch(
            dest,
            input.bundle_path,
            &[&format!("+{}:{}", input.commit, refname)],
            None,
        )?;
        if repo.refname_to_id(&refname)? != input.commit {
            return Err(GitError::Invalid(format!(
                "input ref {} did not resolve to {}",
                input.name, input.commit
            )));
        }
    }
    // The remote may have been removed already by clone_no_checkout; make it explicit.
    if !repo.remotes()?.is_empty() {
        return Err(GitError::Invalid(
            "checkout unexpectedly has remotes".into(),
        ));
    }
    Ok(repo)
}
