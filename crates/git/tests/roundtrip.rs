// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! End-to-end Git flow without any forge: source repo → bare cache → snapshot bundle → agent
//! checkout → commits → series collection → series bundle → cache → inspection → re-creation →
//! push to a bare "forge" repository.

use std::fs;
use std::path::Path;

use artidezi_git::cache::BareCache;
use artidezi_git::publish::{ContentRules, inspect, recreate, recreate_rebased};
use artidezi_git::series::{CollectLimits, collect};
use artidezi_git::workspace::{InputRefSpec, prepare_checkout};
use artidezi_git::{Identity, bundle};
use git2::{Oid, Repository, Signature};

fn commit_file(repo: &Repository, path: &str, content: &str, message: &str) -> Oid {
    let workdir = repo.workdir().unwrap();
    let full = workdir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("Agent", "agent@invalid").unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .unwrap()
}

fn limits() -> CollectLimits {
    CollectLimits {
        max_commits: 10,
        max_commit_message_bytes: 1024,
    }
}

#[test]
fn rebased_recreation_applies_a_clean_series_to_a_new_base() {
    let dir = tempfile::tempdir().unwrap();
    let source = Repository::init_opts(
        dir.path().join("source"),
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    let base = commit_file(&source, "README.md", "base", "Base");
    let patch = commit_file(&source, "README.md", "adapted", "Adapt");
    source
        .reference("refs/heads/target", base, true, "target")
        .unwrap();
    source.set_head("refs/heads/target").unwrap();
    source.checkout_head(None).unwrap();
    let target = commit_file(&source, "unrelated.txt", "unrelated", "Unrelated");

    let cache = BareCache::open_or_init(&dir.path().join("cache")).unwrap();
    cache
        .fetch(
            source.path().to_str().unwrap(),
            &["+refs/heads/*:refs/heads/*"],
            None,
        )
        .unwrap();
    let bot = Identity {
        name: "ArtiDezi".into(),
        email: "bot@invalid".into(),
    };
    let recreated = recreate_rebased(cache.repo(), target, &[patch], &bot).unwrap();
    let commit = cache.repo().find_commit(recreated[0]).unwrap();
    assert_eq!(commit.parent_id(0).unwrap(), target);
    let tree = commit.tree().unwrap();
    let readme = cache
        .repo()
        .find_blob(tree.get_name("README.md").unwrap().id())
        .unwrap();
    assert_eq!(readme.content(), b"adapted");
    assert!(tree.get_name("unrelated.txt").is_some());
}

#[test]
fn push_lease_refuses_a_stale_head_and_an_unexpected_existing_branch() {
    let dir = tempfile::tempdir().unwrap();
    let source = Repository::init_opts(
        dir.path().join("source"),
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    let first = commit_file(&source, "README.md", "first", "First");
    let second = commit_file(&source, "README.md", "second", "Second");
    let cache = BareCache::open_or_init(&dir.path().join("cache")).unwrap();
    cache
        .fetch(
            source.path().to_str().unwrap(),
            &["refs/heads/main:refs/heads/main"],
            None,
        )
        .unwrap();
    let remote = Repository::init_bare(dir.path().join("remote.git")).unwrap();
    let url = remote.path().to_str().unwrap();
    cache
        .push_with_lease(url, "refs/heads/main:refs/heads/main", None, Oid::ZERO_SHA1)
        .unwrap();

    // A branch discovered after preflight must not be overwritten, even by a fast-forward.
    assert!(
        cache
            .push_with_lease(url, "refs/heads/main:refs/heads/main", None, Oid::ZERO_SHA1)
            .is_err()
    );
    cache.set_ref("refs/heads/main", first, "revision").unwrap();
    assert!(
        cache
            .push_with_lease(url, "+refs/heads/main:refs/heads/main", None, first)
            .is_err()
    );
    assert_eq!(remote.refname_to_id("refs/heads/main").unwrap(), second);
    assert!(
        cache
            .push(url, "+refs/heads/main:refs/heads/main", None)
            .is_err()
    );
    cache
        .push_with_lease(url, "+refs/heads/main:refs/heads/main", None, second)
        .unwrap();
    assert_eq!(remote.refname_to_id("refs/heads/main").unwrap(), first);
}

#[test]
fn full_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Upstream repository, standing in for the forge.
    let upstream_dir = root.join("upstream");
    let upstream = Repository::init_opts(
        &upstream_dir,
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    commit_file(&upstream, "README.md", "hello\n", "Initial");
    let base = commit_file(
        &upstream,
        "defaults/main.yml",
        "version: 1.0.0\n",
        "Add defaults",
    );
    let forge_dir = root.join("forge.git");
    Repository::init_bare(&forge_dir).unwrap();
    let forge_url = forge_dir.to_str().unwrap();

    // Orchestrator cache fetches from the upstream working repository (local path, no creds).
    let cache = BareCache::open_or_init(&root.join("cache")).unwrap();
    cache
        .fetch(
            upstream_dir.to_str().unwrap(),
            &["+refs/heads/main:refs/heads/main"],
            None,
        )
        .unwrap();
    assert_eq!(cache.resolve("refs/heads/main").unwrap(), base);

    // A Renovate-style input ref: one more commit on top of base, supplied read-only.
    let renovate_head = commit_file(
        &upstream,
        "defaults/main.yml",
        "version: 1.1.0\n",
        "Update to 1.1.0",
    );
    cache
        .fetch(
            upstream_dir.to_str().unwrap(),
            &["+refs/heads/main:refs/artidezi/inputs/renovate"],
            None,
        )
        .unwrap();
    cache
        .set_ref("refs/heads/main", base, "pin to task base")
        .unwrap();

    // Snapshot bundle at base and input bundle for the extra commit.
    let snapshot = root.join("snapshot.bundle");
    bundle::create(cache.path(), &snapshot, &["refs/heads/main"]).unwrap();
    let input_bundle = root.join("input.bundle");
    bundle::create(
        cache.path(),
        &input_bundle,
        &[&format!("{base}..refs/artidezi/inputs/renovate")],
    )
    .unwrap();

    // Worker prepares the agent's checkout.
    let checkout_dir = root.join("workspace/repos/role");
    let checkout = prepare_checkout(
        &snapshot,
        &checkout_dir,
        "main",
        base,
        &[InputRefSpec {
            name: "renovate",
            commit: renovate_head,
            bundle_path: &input_bundle,
        }],
    )
    .unwrap();
    assert_eq!(checkout.head().unwrap().target().unwrap(), base);
    assert_eq!(
        checkout
            .refname_to_id("refs/artidezi/inputs/renovate")
            .unwrap(),
        renovate_head
    );
    assert!(checkout.remotes().unwrap().is_empty());
    assert!(checkout_dir.join("defaults/main.yml").is_file());

    // The agent makes two commits and leaves an untracked scratch file.
    let c1 = commit_file(
        &checkout,
        "defaults/main.yml",
        "version: 1.1.0\n",
        "Update to 1.1.0",
    );
    let c2 = commit_file(&checkout, "docs/notes.md", "adapted\n", "Adapt docs");
    fs::write(checkout_dir.join("scratch.log"), "junk").unwrap();

    let collected = collect(&checkout_dir, "main", base, limits()).unwrap();
    assert!(collected.is_valid(), "{:?}", collected.problems);
    assert_eq!(collected.head, c2);
    assert_eq!(
        collected.commits.iter().map(|c| c.oid).collect::<Vec<_>>(),
        vec![c1, c2]
    );
    assert_eq!(collected.commits[0].parent, base);
    assert_eq!(collected.commits[1].message, "Adapt docs");
    assert_eq!(collected.untracked_files_count, 1);
    assert!(collected.dirty_tracked_files.is_empty());

    // Series bundle goes back to the orchestrator cache under an attempt ref.
    let series_bundle = root.join("series.bundle");
    bundle::create(
        &checkout_dir,
        &series_bundle,
        &[&format!("{base}..refs/heads/main")],
    )
    .unwrap();
    bundle::verify(cache.path(), &series_bundle, None).unwrap();
    bundle::fetch(
        cache.path(),
        &series_bundle,
        &["+refs/heads/main:refs/artidezi/attempts/att_1/role"],
        None,
    )
    .unwrap();
    assert_eq!(
        cache.resolve("refs/artidezi/attempts/att_1/role").unwrap(),
        c2
    );

    // Inspection with path restrictions.
    let rules = ContentRules {
        allowed_paths: None,
        max_file_bytes: 1024,
    };
    assert!(
        inspect(cache.repo(), base, &[c1, c2], &rules)
            .unwrap()
            .is_empty()
    );
    let strict = ContentRules {
        allowed_paths: Some(vec!["defaults/**".into()]),
        max_file_bytes: 1024,
    };
    let problems = inspect(cache.repo(), base, &[c1, c2], &strict).unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("docs/notes.md"));
    let wrong_order = inspect(cache.repo(), base, &[c2, c1], &rules).unwrap();
    assert!(wrong_order[0].contains("does not follow"));

    // Re-creation with the bot identity, then push to the forge.
    let bot = Identity {
        name: "ArtiDezi".into(),
        email: "bot@invalid".into(),
    };
    let recreated = recreate(cache.repo(), base, &[c1, c2], &bot).unwrap();
    assert_eq!(recreated.len(), 2);
    assert_ne!(recreated, vec![c1, c2]);
    let last = cache.repo().find_commit(recreated[1]).unwrap();
    assert_eq!(last.author().name().unwrap(), "ArtiDezi");
    assert_eq!(last.message().unwrap(), "Adapt docs");
    assert_eq!(
        last.tree_id(),
        cache.repo().find_commit(c2).unwrap().tree_id()
    );
    assert_eq!(last.parent_id(0).unwrap(), recreated[0]);

    cache
        .set_ref("refs/artidezi/publish/att_1", recreated[1], "publish")
        .unwrap();
    cache
        .push(
            forge_url,
            "refs/artidezi/publish/att_1:refs/heads/artidezi/tsk_1",
            None,
        )
        .unwrap();
    let forge = Repository::open_bare(&forge_dir).unwrap();
    assert_eq!(
        forge.refname_to_id("refs/heads/artidezi/tsk_1").unwrap(),
        recreated[1]
    );

    // A non-fast-forward push is refused rather than forced.
    cache
        .set_ref("refs/artidezi/publish/att_2", recreated[0], "publish older")
        .unwrap();
    assert!(
        cache
            .push(
                forge_url,
                "refs/artidezi/publish/att_2:refs/heads/artidezi/tsk_1",
                None
            )
            .is_err()
    );
}

#[test]
fn collection_reports_problems() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init_opts(
        dir.path(),
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    let base = commit_file(&repo, "a", "1", "base");
    commit_file(&repo, "a", "2", "change");
    fs::write(dir.path().join("a"), "3").unwrap();
    let collected = collect(dir.path(), "main", base, limits()).unwrap();
    assert_eq!(collected.dirty_tracked_files, vec!["a".to_string()]);
    assert!(!collected.is_valid());

    fs::write(dir.path().join("a"), "2").unwrap();
    let collected = collect(dir.path(), "other", base, limits()).unwrap();
    assert!(collected.problems[0].contains("instead of refs/heads/other"));

    // Too many commits.
    for i in 0..3 {
        commit_file(&repo, "a", &format!("{i}"), "more");
    }
    let collected = collect(
        dir.path(),
        "main",
        base,
        CollectLimits {
            max_commits: 2,
            max_commit_message_bytes: 1024,
        },
    )
    .unwrap();
    assert!(
        collected
            .problems
            .iter()
            .any(|p| p.contains("exceeds 2 commits"))
    );

    // Base rewritten away: walk never reaches it.
    let unrelated = Oid::from_str(&"9".repeat(40)).unwrap();
    let collected = collect(dir.path(), "main", unrelated, limits()).unwrap();
    assert!(!collected.is_valid());
}

#[test]
fn inspection_reports_a_non_canonical_file_mode_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let base = commit_file(&repo, "a.txt", "a\n", "base");
    let blob = repo.blob(b"target").unwrap();
    let mut entry = b"120777 link\0".to_vec();
    entry.extend_from_slice(blob.as_bytes());
    let tree = repo
        .odb()
        .unwrap()
        .write(git2::ObjectType::Tree, &entry)
        .unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let sig = Signature::now("Agent", "agent@invalid").unwrap();
    let parent = repo.find_commit(base).unwrap();
    let commit = repo
        .commit(None, &sig, &sig, "odd mode", &tree, &[&parent])
        .unwrap();

    let problems = inspect(
        &repo,
        base,
        &[commit],
        &ContentRules {
            allowed_paths: None,
            max_file_bytes: 1024,
        },
    )
    .unwrap();

    assert!(problems.iter().any(|p| p.contains("invalid file mode")));
}

#[test]
fn bundle_operations_stop_at_their_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    commit_file(&repo, "a.txt", "a\n", "base");
    let path = dir.path().join("series.bundle");
    bundle::create(dir.path(), &path, &["HEAD"]).unwrap();

    let error = bundle::verify(dir.path(), &path, Some(std::time::Duration::ZERO)).unwrap_err();

    assert!(error.to_string().contains("timed out"), "{error}");
}
