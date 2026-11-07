// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Collection of the linear commit series an agent left in a checkout.

use std::path::Path;

use git2::{Oid, Repository, Status, StatusOptions};

use crate::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub oid: Oid,
    pub parent: Oid,
    pub message: String,
    pub author: String,
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    pub head: Oid,
    pub on_configured_branch: bool,
    /// Oldest first. Empty when nothing was committed or the walk failed.
    pub commits: Vec<CommitInfo>,
    /// Why the series is not usable, if it is not.
    pub problems: Vec<String>,
    pub dirty_tracked_files: Vec<String>,
    pub untracked_files_count: u64,
}

impl Collected {
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Limits applied during collection. Exceeding one stops the walk and marks the series oversized.
#[derive(Clone, Copy, Debug)]
pub struct CollectLimits {
    pub max_commits: u32,
    pub max_commit_message_bytes: u64,
}

/// Walks first-parent from HEAD down to `base`, requiring every commit to have exactly one parent.
pub fn collect(
    repo_dir: &Path,
    branch: &str,
    base: Oid,
    limits: CollectLimits,
) -> Result<Collected> {
    let repo = Repository::open(repo_dir)?;
    let head_ref = repo.head()?;
    let head = head_ref
        .target()
        .ok_or_else(|| crate::GitError::Invalid("HEAD is not a direct reference".into()))?;
    let head_name = head_ref.name().ok().map(str::to_owned);
    let expected_ref = format!("refs/heads/{branch}");
    let on_configured_branch = head_name.as_deref() == Some(expected_ref.as_str());

    let mut collected = Collected {
        head,
        on_configured_branch,
        commits: vec![],
        problems: vec![],
        dirty_tracked_files: vec![],
        untracked_files_count: 0,
    };
    if !on_configured_branch {
        collected.problems.push(format!(
            "HEAD is {} instead of refs/heads/{branch}",
            head_name.as_deref().unwrap_or("detached")
        ));
    }

    // Working tree state.
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    for entry in repo.statuses(Some(&mut options))?.iter() {
        let status = entry.status();
        if status.intersects(Status::WT_NEW) {
            collected.untracked_files_count += 1;
        } else if status.intersects(
            Status::WT_MODIFIED
                | Status::WT_DELETED
                | Status::WT_TYPECHANGE
                | Status::WT_RENAMED
                | Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE,
        ) {
            collected
                .dirty_tracked_files
                .push(entry.path().unwrap_or("<non-utf8 path>").to_owned());
        }
    }
    if !collected.dirty_tracked_files.is_empty() {
        collected.problems.push(format!(
            "working tree has {} modified tracked file(s)",
            collected.dirty_tracked_files.len()
        ));
    }

    // The series itself.
    let mut current = head;
    let mut chain = vec![];
    loop {
        if current == base {
            break;
        }
        if chain.len() as u32 >= limits.max_commits {
            collected
                .problems
                .push(format!("series exceeds {} commits", limits.max_commits));
            break;
        }
        let commit = match repo.find_commit(current) {
            Ok(c) => c,
            Err(_) => {
                collected
                    .problems
                    .push(format!("commit {current} is missing"));
                break;
            }
        };
        if commit.parent_count() != 1 {
            collected.problems.push(format!(
                "commit {current} has {} parents; the series must be linear and rooted at the base",
                commit.parent_count()
            ));
            break;
        }
        let parent = commit.parent_id(0)?;
        let message = commit.message_raw().unwrap_or("").to_owned();
        if message.len() as u64 > limits.max_commit_message_bytes {
            collected.problems.push(format!(
                "commit {current} message exceeds {} bytes",
                limits.max_commit_message_bytes
            ));
            break;
        }
        let parent_tree = repo.find_commit(parent)?.tree()?;
        let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&commit.tree()?), None)?;
        let stats = diff.stats()?;
        let author = commit.author();
        chain.push(CommitInfo {
            oid: current,
            parent,
            message,
            author: format!(
                "{} <{}>",
                author.name().unwrap_or("?"),
                author.email().unwrap_or("?")
            ),
            files_changed: stats.files_changed() as u64,
            insertions: stats.insertions() as u64,
            deletions: stats.deletions() as u64,
        });
        current = parent;
    }
    chain.reverse();
    collected.commits = chain;
    Ok(collected)
}

/// The patch from `base` to `head`, for human review.
pub fn patch(repo: &Repository, base: Oid, head: Oid) -> Result<Vec<u8>> {
    let base_tree = repo.find_commit(base)?.tree()?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;
    let mut patch = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            patch.push(line.origin() as u8);
        }
        patch.extend_from_slice(line.content());
        true
    })?;
    Ok(patch)
}
