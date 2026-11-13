// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Forge contract suite against a real Forgejo. Runs only when `ARTIDEZI_FORGEJO_URL`,
//! `ARTIDEZI_FORGEJO_TOKEN` and `ARTIDEZI_FORGEJO_USER` are set; `scripts/forgejo-test.sh` starts a
//! throwaway container and sets them. Every run creates its own private repository.

use std::fs;
use std::path::Path;

use artidezi_forge::forgejo::ForgejoForge;
use artidezi_forge::*;
use artidezi_git::cache::BareCache;
use git2::{Oid, Repository, Signature};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn commit_file(repo: &Repository, path: &str, content: &str, message: &str) -> Oid {
    let workdir = repo.workdir().unwrap();
    let full = workdir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("Test", "test@invalid").unwrap();
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

/// A raw API call as another Forgejo user, for effects ArtiDezi itself never performs.
fn api_as(url: &str, token: &str, method: &str, path: &str, body: serde_json::Value) -> String {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let uri = format!("{url}/api/v1{path}");
    let auth = format!("token {token}");
    let mut response = match method {
        "POST" => agent
            .post(&uri)
            .header("Authorization", &auth)
            .send_json(body),
        "PUT" => agent
            .put(&uri)
            .header("Authorization", &auth)
            .send_json(body),
        _ => unreachable!(),
    }
    .unwrap();
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap();
    assert!(
        (200..300).contains(&status),
        "{method} {path}: {status} {text}"
    );
    text
}

fn other_user(prefix: &str) -> (String, String) {
    (
        env(&format!("ARTIDEZI_FORGEJO_{prefix}_USER"))
            .expect("run through scripts/forgejo-test.sh"),
        env(&format!("ARTIDEZI_FORGEJO_{prefix}_TOKEN"))
            .expect("run through scripts/forgejo-test.sh"),
    )
}

#[test]
fn forgejo_contract() {
    let (Some(url), Some(token), Some(user)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let forge = ForgejoForge::new(&url, &token, &user);
    let name = format!("artidezi-test-{}", std::process::id());
    let remote = forge.create_repository(&name, "main").unwrap();
    assert_eq!(remote, format!("{user}/{name}"));

    // Push a base and a feature branch through the orchestrator's cache, as publication does.
    let dir = tempfile::tempdir().unwrap();
    let work = Repository::init_opts(
        dir.path().join("work"),
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    let base = commit_file(&work, "README.md", "hello\n", "Initial");
    let feature = commit_file(
        &work,
        "defaults/main.yml",
        "version: 1.1.0\n",
        "Update to 1.1.0",
    );
    let cache = BareCache::open_or_init(&dir.path().join("cache")).unwrap();
    cache
        .fetch(
            work.path().to_str().unwrap(),
            &["+refs/heads/main:refs/heads/main"],
            None,
        )
        .unwrap();
    cache.set_ref("refs/artidezi/base", base, "").unwrap();
    let creds = forge.git_credentials(&remote).unwrap();
    // auto_init created an initial commit on main; our base replaces it (force) to keep the test
    // self-contained. Real publication never forces target branches.
    cache
        .push_with_lease(
            &forge.clone_url(&remote),
            "+refs/artidezi/base:refs/heads/main",
            creds.as_ref(),
            Oid::from_str(forge.branch_head(&remote, "main").unwrap().as_str()).unwrap(),
        )
        .unwrap();
    cache
        .push(
            &forge.clone_url(&remote),
            "refs/heads/main:refs/heads/artidezi/test",
            creds.as_ref(),
        )
        .unwrap();

    assert_eq!(
        forge.branch_head(&remote, "main").unwrap().as_str(),
        base.to_string()
    );
    assert_eq!(
        forge
            .branch_head(&remote, "artidezi/test")
            .unwrap()
            .as_str(),
        feature.to_string()
    );
    assert!(matches!(
        forge.branch_head(&remote, "nope"),
        Err(ForgeError::NotFound { .. })
    ));

    let mr = forge
        .create_merge_request(
            &remote,
            &NewMergeRequest {
                title: "Update to 1.1.0".into(),
                body: "body v1".into(),
                source_branch: "artidezi/test".into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap();
    assert_eq!(mr.author_login, user);
    assert_eq!(mr.head_commit.as_str(), feature.to_string());
    assert_eq!(mr.state, MergeRequestState::Open);
    assert_eq!(mr.checks, ChecksState::Absent);
    assert!(mr.url.starts_with(&url), "{}", mr.url);
    let review = forge
        .create_comment_review(
            &remote,
            mr.number,
            &mr.head_commit,
            "Informational assessment only.",
        )
        .unwrap();
    assert_eq!(review.author_login, user);
    assert_eq!(
        forge.list_reviews(&remote, mr.number).unwrap(),
        vec![review.clone()]
    );
    // Another reviewer's decisions are visible to the merge gate, including a later dismissal.
    let (maintainer, maintainer_token) = other_user("MAINTAINER");
    forge.add_collaborator(&remote, &maintainer).unwrap();
    ForgejoForge::new(&url, &maintainer_token, &maintainer)
        .submit_review_for_test(
            &remote,
            mr.number,
            &mr.head_commit,
            "REQUEST_CHANGES",
            "Needs a fix.",
        )
        .unwrap();
    let reviews = forge.list_reviews(&remote, mr.number).unwrap();
    assert_eq!(reviews.len(), 2, "{reviews:?}");
    assert_eq!(reviews[0], review);
    assert_eq!(reviews[1].author_login, maintainer);
    assert_eq!(reviews[1].state, ReviewState::ChangesRequested);
    api_as(
        &url,
        &token,
        "POST",
        &format!(
            "/repos/{remote}/pulls/{}/reviews/{}/dismissals",
            mr.number, reviews[1].id
        ),
        serde_json::json!({"message": "Addressed."}),
    );
    assert_eq!(
        forge.list_reviews(&remote, mr.number).unwrap()[1].state,
        ReviewState::Dismissed
    );
    assert_eq!(
        forge
            .proposals_for_branch(&remote, "artidezi/test")
            .unwrap()
            .len(),
        1
    );
    // Source snapshots fetch the provider's PR ref from the configured target repository.
    let pr_ref = forge.merge_request_ref(&remote, mr.number).unwrap();
    cache
        .fetch(
            &forge.clone_url(&remote),
            &[&format!("+{pr_ref}:refs/artidezi/source")],
            forge.git_credentials(&remote).unwrap().as_ref(),
        )
        .unwrap();
    assert_eq!(
        Repository::open_bare(cache.path())
            .unwrap()
            .refname_to_id("refs/artidezi/source")
            .unwrap(),
        feature
    );

    forge
        .update_merge_request(&remote, mr.number, "Update to 1.1.0 (revised)", "body v2")
        .unwrap();
    let fetched = forge.get_merge_request(&remote, mr.number).unwrap();
    assert_eq!(fetched.title, "Update to 1.1.0 (revised)");
    assert_eq!(fetched.body, "body v2");

    cache
        .push(
            &forge.clone_url(&remote),
            "refs/heads/main:refs/heads/artidezi/superseded",
            creds.as_ref(),
        )
        .unwrap();
    let superseded = forge
        .create_merge_request(
            &remote,
            &NewMergeRequest {
                title: "Superseded adaptation".into(),
                body: "body".into(),
                source_branch: "artidezi/superseded".into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap();
    let expected = ExpectedProposal {
        number: superseded.number,
        source_branch: "artidezi/superseded",
        target_branch: "main",
        head: &superseded.head_commit,
    };
    let older = artidezi_contract::common::CommitSha(base.to_string());
    for wrong in [
        ExpectedProposal {
            head: &older,
            ..expected
        },
        ExpectedProposal {
            source_branch: "artidezi/test",
            ..expected
        },
        ExpectedProposal {
            target_branch: "artidezi/test",
            ..expected
        },
    ] {
        assert!(
            forge
                .close_merge_request(&remote, &wrong, "Superseded.")
                .is_err()
        );
    }
    assert_eq!(
        forge
            .get_merge_request(&remote, superseded.number)
            .unwrap()
            .state,
        MergeRequestState::Open
    );
    forge
        .close_merge_request(&remote, &expected, "Superseded by a newer source.")
        .unwrap();
    assert_eq!(
        forge
            .get_merge_request(&remote, superseded.number)
            .unwrap()
            .state,
        MergeRequestState::Closed
    );
    assert!(
        forge
            .close_merge_request(&remote, &expected, "Again.")
            .is_err()
    );

    let open = forge
        .list_open_merge_requests(&remote, Some(&user))
        .unwrap();
    assert_eq!(open.len(), 1);
    assert!(
        forge
            .list_open_merge_requests(&remote, Some("someone-else"))
            .unwrap()
            .is_empty()
    );

    forge
        .set_commit_status(&remote, &feature.to_string(), "success", "ci/test")
        .unwrap();
    assert_eq!(
        forge.get_merge_request(&remote, mr.number).unwrap().checks,
        ChecksState::Success
    );

    let stale = artidezi_contract::common::CommitSha(base.to_string());
    assert!(matches!(
        forge.merge(&remote, mr.number, &stale, MergeMethod::Rebase),
        Err(ForgeError::HeadMoved { .. })
    ));
    // Review/status changes queue Forgejo's asynchronous mergeability recalculation.
    let mut merged = None;
    for _ in 0..30 {
        match forge.merge(&remote, mr.number, &mr.head_commit, MergeMethod::Rebase) {
            Ok(head) => {
                merged = Some(head);
                break;
            }
            Err(ForgeError::NotMergeable(_, reason))
                if reason.contains("Please try again later") =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            other => panic!("unexpected fixture merge result: {other:?}"),
        }
    }
    let merged = merged.expect("Forgejo mergeability should settle within three seconds");
    assert_eq!(
        merged.as_str(),
        feature.to_string(),
        "rebase of a fast-forwardable branch keeps the commit"
    );
    assert_eq!(
        forge.get_merge_request(&remote, mr.number).unwrap().state,
        MergeRequestState::Merged
    );
    assert!(
        forge
            .list_open_merge_requests(&remote, None)
            .unwrap()
            .is_empty()
    );

    forge.create_tag(&remote, "v1.1.0-0", &merged).unwrap();
    assert!(forge.create_tag(&remote, "v1.1.0-0", &merged).is_err());
    let push_credentials = forge.git_push_credentials(&remote).unwrap();
    cache
        .push_with_lease(
            &forge.clone_url(&remote),
            ":refs/heads/artidezi/test",
            push_credentials.as_ref(),
            feature,
        )
        .unwrap();
    // A lost deletion response can be retried with the original lease.
    cache
        .push_with_lease(
            &forge.clone_url(&remote),
            ":refs/heads/artidezi/test",
            push_credentials.as_ref(),
            feature,
        )
        .unwrap();
    assert!(matches!(
        forge.branch_head(&remote, "artidezi/test"),
        Err(ForgeError::NotFound { .. })
    ));
}

#[test]
fn forgejo_source_approval_and_merge_contract() {
    let (Some(url), Some(token), Some(user)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let (renovate_user, renovate_token) = other_user("RENOVATE");
    let plain = ForgejoForge::new(&url, &token, &user);
    let remote = plain
        .create_repository(&format!("artidezi-actions-{}", std::process::id()), "main")
        .unwrap();
    plain.add_collaborator(&remote, &renovate_user).unwrap();
    let forge = ForgejoForge::new(&url, &token, &user).with_source_actions(
        [remote.to_uppercase()],
        [(remote.clone(), renovate_user.to_uppercase())]
            .into_iter()
            .collect(),
    );
    // Merge authority names authors, not repositories: another author is refused even when green.
    let mut other_authors = SourceMergeAuthors::default();
    other_authors.allow(&remote, MergeAuthor::Login("someone-else".into()));
    other_authors.allow(
        &remote,
        MergeAuthor::Pattern(regex::Regex::new("^(?i:someone-.*)$").unwrap()),
    );
    let other_author =
        ForgejoForge::new(&url, &token, &user).with_source_actions([remote.clone()], other_authors);
    let renovate = ForgejoForge::new(&url, &renovate_token, &renovate_user);

    // The bot pushes a branch; the Renovate-style user opens the pull request from it.
    let dir = tempfile::tempdir().unwrap();
    let cache = BareCache::open_or_init(&dir.path().join("cache")).unwrap();
    let creds = plain.git_credentials(&remote).unwrap();
    cache
        .fetch(
            &plain.clone_url(&remote),
            &["+refs/heads/main:refs/heads/main"],
            creds.as_ref(),
        )
        .unwrap();
    let repo = Repository::open_bare(cache.path()).unwrap();
    let parent = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut tree = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    tree.insert("lib.txt", repo.blob(b"2.0\n").unwrap(), 0o100644)
        .unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let sig = Signature::now("Renovate", "renovate@invalid").unwrap();
    let update = repo
        .commit(None, &sig, &sig, "Update lib", &tree, &[&parent])
        .unwrap();
    cache
        .set_ref("refs/heads/renovate/lib", update, "")
        .unwrap();
    cache
        .push(
            &plain.clone_url(&remote),
            "refs/heads/renovate/lib:refs/heads/renovate/lib",
            creds.as_ref(),
        )
        .unwrap();
    let pull = renovate
        .create_merge_request(
            &remote,
            &NewMergeRequest {
                title: "Update lib".into(),
                body: "Automated update.".into(),
                source_branch: "renovate/lib".into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap();
    assert_eq!(pull.author_login, renovate_user);
    let head = pull.head_commit.clone();
    let stale = artidezi_contract::common::CommitSha(parent.id().to_string());

    // Without configured authority, or without green checks, nothing is approved or merged.
    assert!(
        plain
            .create_approval_review(&remote, pull.number, &head, "Approve.")
            .is_err()
    );
    assert!(
        plain
            .merge(&remote, pull.number, &head, MergeMethod::Merge)
            .is_err()
    );
    assert!(
        forge
            .create_approval_review(&remote, pull.number, &head, "Approve.")
            .is_err()
    );
    assert!(
        forge
            .merge(&remote, pull.number, &head, MergeMethod::Merge)
            .is_err()
    );
    plain
        .set_commit_status(&remote, head.as_str(), "pending", "ci/slow")
        .unwrap();
    plain
        .set_commit_status(&remote, head.as_str(), "success", "ci/test")
        .unwrap();
    assert_eq!(
        forge
            .get_merge_request(&remote, pull.number)
            .unwrap()
            .checks,
        ChecksState::Pending
    );
    assert!(
        forge
            .create_approval_review(&remote, pull.number, &head, "Approve.")
            .is_err()
    );
    plain
        .set_commit_status(&remote, head.as_str(), "success", "ci/slow")
        .unwrap();
    assert!(forge.list_reviews(&remote, pull.number).unwrap().is_empty());
    // Only the latest status of each context is reported.
    let mut results: Vec<_> = forge
        .get_merge_request(&remote, pull.number)
        .unwrap()
        .check_results
        .unwrap()
        .into_iter()
        .map(|r| (r.kind, r.name, r.status, r.conclusion))
        .collect();
    results.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        results,
        ["ci/slow", "ci/test"].map(|name| (
            CheckResultKind::CommitStatus,
            name.to_owned(),
            "success".to_owned(),
            None
        ))
    );

    // An approval names the exact head, and a stale expected head refuses the merge.
    assert!(
        forge
            .create_approval_review(&remote, pull.number, &stale, "Approve.")
            .is_err()
    );
    let approval = forge
        .create_approval_review(&remote, pull.number, &head, "Approve.")
        .unwrap();
    assert_eq!(approval.state, ReviewState::Approved);
    assert_eq!(approval.head_commit, head);
    assert_eq!(approval.author_login, user);
    assert_eq!(
        forge.list_reviews(&remote, pull.number).unwrap(),
        vec![approval]
    );
    assert!(matches!(
        forge.merge(&remote, pull.number, &stale, MergeMethod::Merge),
        Err(ForgeError::HeadMoved { .. })
    ));
    assert!(
        other_author
            .merge(&remote, pull.number, &head, MergeMethod::Merge)
            .is_err()
    );
    let merged = forge
        .merge(&remote, pull.number, &head, MergeMethod::Merge)
        .unwrap();
    assert_eq!(forge.branch_head(&remote, "main").unwrap(), merged);
    assert_ne!(merged, head, "a merge commit, not a fast-forward");
    let info = forge.get_merge_request(&remote, pull.number).unwrap();
    assert_eq!(info.state, MergeRequestState::Merged);
    assert_eq!(info.merge_commit, Some(merged));
    assert!(matches!(
        forge.merge(&remote, pull.number, &head, MergeMethod::Merge),
        Err(ForgeError::NotMergeable(..))
    ));
}
