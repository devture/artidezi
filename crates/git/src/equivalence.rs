// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Whether two heads of a source branch carry the same change against their targets.
//!
//! A source branch rebased onto a newer target keeps its change while its commits get new ids.
//! The change is compared as the exact set of paths it touches, with their object ids and modes
//! before and after, measured from each head's merge base with its own target. There is no rename
//! detection and no textual comparison: whitespace, binary content and mode changes all count.
//! A rebase that also absorbs target edits to a file the source touches therefore looks
//! different, which is the safe direction.

use git2::{Delta, DiffOptions, Oid, Repository};

use crate::Result;

/// One changed path, compared byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ChangedPath {
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
    old_id: Oid,
    new_id: Oid,
    old_mode: u32,
    new_mode: u32,
}

/// The change a head introduces relative to its merge base with a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceChange {
    paths: Vec<ChangedPath>,
}

impl SourceChange {
    /// `None` when the change cannot be determined unambiguously: no single merge base, or nothing
    /// changed. Missing objects are errors. Callers treat both as "not provably the same".
    pub fn of(repo: &Repository, target: Oid, head: Oid) -> Result<Option<Self>> {
        repo.find_commit(target)?;
        let new = repo.find_commit(head)?.tree()?;
        let bases = match repo.merge_bases(target, head) {
            Ok(bases) => bases,
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bases = bases.iter().copied().collect::<Vec<_>>();
        bases.dedup();
        let [base] = bases[..] else {
            return Ok(None);
        };
        let old = repo.find_commit(base)?.tree()?;
        let mut options = DiffOptions::new();
        options.include_typechange(true).ignore_submodules(false);
        let diff = repo.diff_tree_to_tree(Some(&old), Some(&new), Some(&mut options))?;
        let odb = repo.odb()?;
        let mut paths = Vec::new();
        for delta in diff.deltas() {
            if !matches!(
                delta.status(),
                Delta::Added | Delta::Deleted | Delta::Modified | Delta::Typechange
            ) {
                return Ok(None);
            }
            let (old_file, new_file) = (delta.old_file(), delta.new_file());
            // Tree diffs compare ids without reading blobs; a change must be fully present.
            for file in [&old_file, &new_file] {
                if !file.id().is_zero()
                    && file.mode() != git2::FileMode::Commit
                    && !odb.exists(file.id())
                {
                    return Err(crate::GitError::Invalid(format!(
                        "object {} is missing",
                        file.id()
                    )));
                }
            }
            paths.push(ChangedPath {
                old_path: old_file.path_bytes().map(<[u8]>::to_vec),
                new_path: new_file.path_bytes().map(<[u8]>::to_vec),
                old_id: old_file.id(),
                new_id: new_file.id(),
                old_mode: u32::from(old_file.mode()),
                new_mode: u32::from(new_file.mode()),
            });
        }
        if paths.is_empty() {
            return Ok(None);
        }
        paths.sort();
        Ok(Some(Self { paths }))
    }
}

/// True only when both heads provably carry the same change: `(target, head)` pairs are each
/// measured against their own merge base. Any ambiguity is reported as `false`.
pub fn same_change(repo: &Repository, before: (Oid, Oid), after: (Oid, Oid)) -> Result<bool> {
    let Some(before) = SourceChange::of(repo, before.0, before.1)? else {
        return Ok(false);
    };
    Ok(SourceChange::of(repo, after.0, after.1)?.is_some_and(|after| after == before))
}

#[cfg(test)]
mod tests {
    use git2::{IndexEntry, IndexTime, Signature};

    use super::*;

    fn commit(repo: &Repository, parents: &[Oid], files: &[(&str, &[u8], u32)]) -> Oid {
        let mut index = git2::Index::new().unwrap();
        for (path, content, mode) in files {
            let entry = IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: *mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: repo.blob(content).unwrap(),
                flags: 0,
                flags_extended: 0,
                path: path.as_bytes().to_vec(),
            };
            index.add(&entry).unwrap();
        }
        let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
        let sig = Signature::now("Test", "test@invalid").unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|p| repo.find_commit(*p).unwrap())
            .collect();
        let parents: Vec<_> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "test", &tree, &parents)
            .unwrap()
    }

    const F: u32 = 0o100644;
    const X: u32 = 0o100755;

    fn repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn a_rebase_over_unrelated_target_changes_keeps_the_change() {
        let (_dir, repo) = repo();
        let t1 = commit(&repo, &[], &[("a", b"1\n", F), ("b", b"x\n", F)]);
        let h1 = commit(&repo, &[t1], &[("a", b"2\n", F), ("b", b"x\n", F)]);
        let t2 = commit(&repo, &[t1], &[("a", b"1\n", F), ("b", b"y\n", F)]);
        let h2 = commit(&repo, &[t2], &[("a", b"2\n", F), ("b", b"y\n", F)]);
        assert!(same_change(&repo, (t1, h1), (t2, h2)).unwrap());
        // Measuring the new head against the old target would include the target's own edit.
        assert!(!same_change(&repo, (t1, h1), (t1, h2)).unwrap());
    }

    #[test]
    fn content_whitespace_and_mode_differences_are_changes() {
        let (_dir, repo) = repo();
        let t = commit(&repo, &[], &[("a", b"1\n", F)]);
        let h = commit(&repo, &[t], &[("a", b"2\n", F)]);
        for files in [
            &[("a", &b"3\n"[..], F)][..],
            &[("a", b"2 \n", F)],
            &[("a", b"2\n", X)],
            &[("a", b"2\n", F), ("c", b"", F)],
        ] {
            let other = commit(&repo, &[t], files);
            assert!(!same_change(&repo, (t, h), (t, other)).unwrap());
        }
        let again = commit(&repo, &[t], &[("a", b"2\n", F)]);
        assert!(same_change(&repo, (t, h), (t, again)).unwrap());
    }

    #[test]
    fn a_rebase_absorbing_target_edits_to_the_same_file_is_not_proven_equal() {
        let (_dir, repo) = repo();
        let t1 = commit(&repo, &[], &[("a", b"1\n2\n", F)]);
        let h1 = commit(&repo, &[t1], &[("a", b"1\nB\n", F)]);
        let t2 = commit(&repo, &[t1], &[("a", b"A\n2\n", F)]);
        let h2 = commit(&repo, &[t2], &[("a", b"A\nB\n", F)]);
        assert!(!same_change(&repo, (t1, h1), (t2, h2)).unwrap());
    }

    #[test]
    fn renames_compare_as_deletion_and_addition() {
        let (_dir, repo) = repo();
        let t = commit(&repo, &[], &[("a", b"1\n", F)]);
        let h1 = commit(&repo, &[t], &[("b", b"1\n", F)]);
        let h2 = commit(&repo, &[t], &[("b", b"1\n", F)]);
        let h3 = commit(&repo, &[t], &[("c", b"1\n", F)]);
        assert!(same_change(&repo, (t, h1), (t, h2)).unwrap());
        assert!(!same_change(&repo, (t, h1), (t, h3)).unwrap());
    }

    #[test]
    fn empty_or_ambiguous_changes_are_never_equal() {
        let (_dir, repo) = repo();
        let t = commit(&repo, &[], &[("a", b"1\n", F)]);
        assert!(!same_change(&repo, (t, t), (t, t)).unwrap());
        let unrelated = commit(&repo, &[], &[("a", b"2\n", F)]);
        assert!(!same_change(&repo, (t, unrelated), (t, unrelated)).unwrap());
        // Criss-cross history gives two merge bases.
        let a = commit(&repo, &[t], &[("a", b"1\n", F), ("x", b"x\n", F)]);
        let b = commit(&repo, &[t], &[("a", b"1\n", F), ("y", b"y\n", F)]);
        let ab = commit(
            &repo,
            &[a, b],
            &[("a", b"1\n", F), ("x", b"x\n", F), ("y", b"y\n", F)],
        );
        let ba = commit(
            &repo,
            &[b, a],
            &[("a", b"1\n", F), ("x", b"x\n", F), ("y", b"y\n", F)],
        );
        let h = commit(
            &repo,
            &[ab],
            &[("a", b"9\n", F), ("x", b"x\n", F), ("y", b"y\n", F)],
        );
        assert!(!same_change(&repo, (ba, h), (ba, h)).unwrap());
    }

    #[test]
    fn missing_objects_are_errors() {
        let (_dir, repo) = repo();
        let t = commit(&repo, &[], &[("a", b"1\n", F)]);
        let h = commit(&repo, &[t], &[("a", b"2\n", F)]);
        let missing = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        assert!(same_change(&repo, (t, h), (t, missing)).is_err());
        let blob = repo.blob(b"gone\n").unwrap();
        let gone = commit(&repo, &[t], &[("a", b"gone\n", F)]);
        let hex = blob.to_string();
        std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
        assert!(same_change(&repo, (t, gone), (t, gone)).is_err());
    }

    #[test]
    fn binary_and_type_changes_compare_by_identity() {
        let (_dir, repo) = repo();
        const L: u32 = 0o120000;
        let t = commit(&repo, &[], &[("a", b"1\n", F), ("bin", b"\0\x01", F)]);
        let h = commit(&repo, &[t], &[("a", b"target", L), ("bin", b"\0\x02", F)]);
        let same = commit(&repo, &[t], &[("a", b"target", L), ("bin", b"\0\x02", F)]);
        let other = commit(&repo, &[t], &[("a", b"target", L), ("bin", b"\0\x03", F)]);
        let plain = commit(&repo, &[t], &[("a", b"target", F), ("bin", b"\0\x02", F)]);
        assert!(same_change(&repo, (t, h), (t, same)).unwrap());
        assert!(!same_change(&repo, (t, h), (t, other)).unwrap());
        assert!(!same_change(&repo, (t, h), (t, plain)).unwrap());
    }
}
