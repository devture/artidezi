// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Orchestrator-side handling of an untrusted commit series: content checks and re-creation with
//! the bot identity. Commits are re-created from their trees inside the bare cache, so untrusted
//! content is never checked out to a filesystem and the agent's own commit objects never reach a
//! forge.

use git2::{Oid, Repository, TreeWalkMode, TreeWalkResult};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{GitError, Identity, Result};

#[derive(Clone, Debug)]
pub struct ContentRules {
    /// Glob patterns; `None` means any path.
    pub allowed_paths: Option<Vec<String>>,
    pub max_file_bytes: u64,
}

const GITLINK_MODE: i32 = 0o160000;
const SYMLINK_MODE: i32 = 0o120000;
/// The tree entry modes Git itself writes. `DiffFile::mode()` panics on anything else, and a
/// worker can craft such an entry, so raw modes are checked before it is called.
const CANONICAL_MODES: [i32; 5] = [0o100644, 0o100664, 0o100755, SYMLINK_MODE, GITLINK_MODE];

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern).map_err(|e| {
                GitError::Invalid(format!("bad allowed_paths glob {pattern:?}: {e}"))
            })?,
        );
    }
    builder
        .build()
        .map_err(|e| GitError::Invalid(e.to_string()))
}

/// Re-walks the series from `base` and checks every commit's content. Returns every problem.
pub fn inspect(
    repo: &Repository,
    base: Oid,
    commits: &[Oid],
    rules: &ContentRules,
) -> Result<Vec<String>> {
    let mut problems = vec![];
    let globs = rules
        .allowed_paths
        .as_deref()
        .map(build_globs)
        .transpose()?;
    let mut expected_parent = base;
    for &oid in commits {
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => {
                problems.push(format!("commit {oid} is not present"));
                break;
            }
        };
        if commit.parent_count() != 1 || commit.parent_id(0)? != expected_parent {
            problems.push(format!("commit {oid} does not follow {expected_parent}"));
            break;
        }
        let parent_tree = repo.find_commit(expected_parent)?.tree()?;
        let tree = commit.tree()?;
        let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
        for delta in diff.deltas() {
            let touched: std::collections::BTreeSet<_> = [delta.old_file(), delta.new_file()]
                .into_iter()
                .filter_map(|file| file.path().map(|p| p.to_path_buf()))
                .collect();
            for path in touched {
                if let Some(globs) = &globs
                    && !globs.is_match(path.as_os_str())
                {
                    problems.push(format!(
                        "commit {oid} touches {}, outside allowed_paths",
                        path.to_string_lossy()
                    ));
                }
            }
            let new_file = delta.new_file();
            if new_file.id().is_zero() {
                continue;
            }
            let path_str = new_file
                .path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let raw_mode = new_file
                .path()
                .and_then(|path| tree.get_path(path).ok())
                .map(|entry| entry.filemode_raw());
            let Some(mode) = raw_mode.filter(|mode| CANONICAL_MODES.contains(mode)) else {
                problems.push(format!(
                    "commit {oid} writes {path_str} with an invalid file mode"
                ));
                continue;
            };
            if mode == GITLINK_MODE {
                problems.push(format!("commit {oid} adds submodule entry {path_str}"));
                continue;
            }
            // The header gives the size without inflating a possibly huge object.
            let size = match repo.odb()?.read_header(new_file.id()) {
                Ok((size, _)) => size as u64,
                Err(_) => {
                    problems.push(format!("commit {oid} writes unreadable object {path_str}"));
                    continue;
                }
            };
            if size > rules.max_file_bytes {
                problems.push(format!(
                    "commit {oid} writes {path_str} with {size} bytes, above the {} byte limit",
                    rules.max_file_bytes
                ));
            } else if mode == SYMLINK_MODE {
                let target = repo.find_blob(new_file.id())?;
                let target = String::from_utf8_lossy(target.content()).into_owned();
                if target.starts_with('/') || target.split('/').any(|c| c == "..") {
                    problems.push(format!(
                        "commit {oid} adds symlink {path_str} pointing outside the tree ({target})"
                    ));
                }
            }
        }
        // Belt and braces: gitlinks that survive unchanged in the tree are already in the base and
        // therefore allowed, but a tree walk catches anything the diff did not classify.
        tree.walk(TreeWalkMode::PreOrder, |_root, entry| {
            if entry.filemode() == GITLINK_MODE
                && diff.deltas().any(|d| d.new_file().id() == entry.id())
            {
                problems.push(format!("commit {oid} contains a gitlink entry"));
            }
            TreeWalkResult::Ok
        })?;
        expected_parent = oid;
    }
    Ok(problems)
}

/// Creates new commits with the same trees and messages but the bot identity, chained on `base`.
/// Returns the new object ids, oldest first. Nothing is checked out.
pub fn recreate(
    repo: &Repository,
    base: Oid,
    commits: &[Oid],
    identity: &Identity,
) -> Result<Vec<Oid>> {
    let signature = identity.signature()?;
    let mut parent = repo.find_commit(base)?;
    let mut created = Vec::with_capacity(commits.len());
    for &oid in commits {
        let original = repo.find_commit(oid)?;
        let tree = original.tree()?;
        let message = original.message_raw().unwrap_or("");
        let new_oid = repo.commit(None, &signature, &signature, message, &tree, &[&parent])?;
        parent = repo.find_commit(new_oid)?;
        created.push(new_oid);
    }
    Ok(created)
}

/// Replays a commit series as a clean three-way merge on top of a newer base.
/// Returns an error when any commit conflicts; nothing is checked out.
pub fn recreate_rebased(
    repo: &Repository,
    base: Oid,
    commits: &[Oid],
    identity: &Identity,
) -> Result<Vec<Oid>> {
    let signature = identity.signature()?;
    let mut parent = repo.find_commit(base)?;
    let mut created = Vec::with_capacity(commits.len());
    for &oid in commits {
        let original = repo.find_commit(oid)?;
        let original_parent = original.parent(0)?;
        let mut index = repo.merge_trees(
            &original_parent.tree()?,
            &parent.tree()?,
            &original.tree()?,
            None,
        )?;
        if index.has_conflicts() {
            return Err(GitError::Invalid(format!(
                "commit {oid} does not apply cleanly on top of {base}"
            )));
        }
        let tree = repo.find_tree(index.write_tree_to(repo)?)?;
        let message = original.message_raw().unwrap_or("");
        let new_oid = repo.commit(None, &signature, &signature, message, &tree, &[&parent])?;
        parent = repo.find_commit(new_oid)?;
        created.push(new_oid);
    }
    Ok(created)
}
