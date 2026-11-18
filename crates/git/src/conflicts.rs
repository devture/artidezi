// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Whether a source head still merges cleanly into a target commit.
//!
//! The merge happens in memory on objects already in the repository: no worktree, hooks,
//! attributes-driven drivers or repository code run. It answers only "would a plain three-way
//! merge conflict"; a clean merge says nothing about whether the result builds or behaves.

use std::collections::BTreeSet;

use git2::{Oid, Repository, Tree};

use crate::Result;

/// Changed files above this size are not merged in memory; see [`conflicting_paths`].
pub const MAX_MERGED_BLOB_BYTES: usize = 16 * 1024 * 1024;

/// Paths that conflict when `head` is merged into `target`, sorted and deduplicated. An empty list
/// means the merge is clean or was not attempted. It is not attempted when the commits share no
/// history or have several merge bases (libgit2 would first merge the bases), or when any file
/// either side changed exceeds [`MAX_MERGED_BLOB_BYTES`]. The merge and its rename detection read
/// only changed files, and sizes come from object headers, so the probe's memory stays bounded.
/// Callers then evaluate the source as usual. Missing objects are errors.
pub fn conflicting_paths(repo: &Repository, target: Oid, head: Oid) -> Result<Vec<String>> {
    let target = repo.find_commit(target)?;
    let head = repo.find_commit(head)?;
    let mut bases = match repo.merge_bases(target.id(), head.id()) {
        Ok(bases) => bases.iter().copied().collect::<Vec<_>>(),
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    bases.dedup();
    let [base] = bases[..] else {
        return Ok(Vec::new());
    };
    let base = repo.find_commit(base)?.tree()?;
    let odb = repo.odb()?;
    for side in [&target, &head] {
        for id in changed_blobs(repo, &base, &side.tree()?)? {
            if odb.read_header(id)?.0 > MAX_MERGED_BLOB_BYTES {
                return Ok(Vec::new());
            }
        }
    }
    let index = repo.merge_commits(&target, &head, None)?;
    if !index.has_conflicts() {
        return Ok(Vec::new());
    }
    let mut paths = BTreeSet::new();
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        for entry in [conflict.ancestor, conflict.our, conflict.their]
            .into_iter()
            .flatten()
        {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    Ok(paths.into_iter().collect())
}

/// Blob ids before and after each change. Submodule commits are compared by tree entry alone and
/// are not objects of this repository, so they are skipped.
fn changed_blobs(repo: &Repository, old: &Tree, new: &Tree) -> Result<BTreeSet<Oid>> {
    let diff = repo.diff_tree_to_tree(Some(old), Some(new), None)?;
    let mut changed = BTreeSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                changed.insert(file.id());
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use git2::Signature;

    use super::*;

    fn commit(repo: &Repository, parents: &[Oid], files: &[(&str, &str)]) -> Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (path, content) in files {
            builder
                .insert(path, repo.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = Signature::now("Test", "test@invalid").unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|p| repo.find_commit(*p).unwrap())
            .collect();
        let parents: Vec<_> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "test", &tree, &parents)
            .unwrap()
    }

    #[test]
    fn a_source_behind_its_target_merges_cleanly_unless_edits_touch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let base = commit(
            &repo,
            &[],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "1\n")],
        );
        let source = commit(
            &repo,
            &[base],
            &[("justfile", "image 2\npin 1\n\nbuild\n"), ("b", "1\n")],
        );
        // Unrelated target work: the source is behind but still merges.
        let unrelated = commit(
            &repo,
            &[base],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "2\n")],
        );
        assert!(
            conflicting_paths(&repo, unrelated, source)
                .unwrap()
                .is_empty()
        );
        // An edit on the adjacent line conflicts, as it does for forges and `git merge`.
        let adjacent = commit(
            &repo,
            &[base],
            &[("justfile", "image 1\npin 2\n\nbuild\n"), ("b", "1\n")],
        );
        assert_eq!(
            conflicting_paths(&repo, adjacent, source).unwrap(),
            ["justfile"]
        );
        // Unrelated histories are not a conflict this check can describe.
        let orphan = commit(&repo, &[], &[("justfile", "other\n")]);
        assert!(conflicting_paths(&repo, orphan, source).unwrap().is_empty());
        // Submodule updates conflict by tree entry; their commits are not in this repository.
        let gitlink = |parents: &[Oid], sub: &str| {
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert("module", Oid::from_str(&sub.repeat(40)).unwrap(), 0o160000)
                .unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = Signature::now("Test", "test@invalid").unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|p| repo.find_commit(*p).unwrap())
                .collect();
            let parents: Vec<_> = parents.iter().collect();
            repo.commit(None, &sig, &sig, "test", &tree, &parents)
                .unwrap()
        };
        let module_base = gitlink(&[], "1");
        let module_source = gitlink(&[module_base], "2");
        let module_target = gitlink(&[module_base], "3");
        assert_eq!(
            conflicting_paths(&repo, module_target, module_source).unwrap(),
            ["module"]
        );
        // Several merge bases would first need a virtual ancestor; that is left to the evaluation.
        let a = commit(
            &repo,
            &[base],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "a\n")],
        );
        let b = commit(
            &repo,
            &[base],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "b\n")],
        );
        let ab = commit(
            &repo,
            &[a, b],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "ab\n")],
        );
        let ba = commit(
            &repo,
            &[b, a],
            &[("justfile", "image 1\npin 1\n\nbuild\n"), ("b", "ba\n")],
        );
        assert!(conflicting_paths(&repo, ab, ba).unwrap().is_empty());
        // A large changed file is left to the evaluation instead of merged here.
        let large = "x\n".repeat(MAX_MERGED_BLOB_BYTES / 2 + 1);
        let large_base = commit(&repo, &[], &[("data", &large)]);
        let large_source = commit(&repo, &[large_base], &[("data", &format!("a{large}"))]);
        let large_target = commit(&repo, &[large_base], &[("data", &format!("b{large}"))]);
        assert!(
            conflicting_paths(&repo, large_target, large_source)
                .unwrap()
                .is_empty()
        );
        let missing = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        assert!(conflicting_paths(&repo, missing, source).is_err());
    }
}
