// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! In-process forge for the offline test suite. Repositories are local bare git repositories the
//! orchestrator's cache fetches from and pushes to over the filesystem; merge requests, checks and
//! tags are kept in memory. Merging really updates the target branch so later tasks see it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use artidezi_contract::common::CommitSha;
use artidezi_git::cache::HttpsCredentials;
use git2::{Oid, Repository};

use crate::*;

#[derive(Clone, Debug)]
struct StoredMergeRequest {
    info: MergeRequestInfo,
}

#[derive(Default)]
struct State {
    next_number: u64,
    /// Keyed by (remote, number).
    merge_requests: BTreeMap<(String, u64), StoredMergeRequest>,
    /// Keyed by (remote, tag name).
    tags: BTreeMap<(String, String), CommitSha>,
    /// Check results a test wants a merge request to report, keyed by (remote, source branch).
    checks: BTreeMap<(String, String), ChecksState>,
    fail_next_create: BTreeSet<String>,
    reviews: BTreeMap<(String, u64), Vec<MergeRequestReview>>,
}

/// Clones share the same in-memory state, so a test can keep a handle after boxing one for the
/// orchestrator.
#[derive(Clone)]
pub struct FakeForge {
    root: PathBuf,
    state: Arc<Mutex<State>>,
}

fn check_results(checks: ChecksState) -> Option<Vec<CheckResult>> {
    let status = match checks {
        ChecksState::Absent => return Some(Vec::new()),
        ChecksState::Pending => "pending",
        ChecksState::Inconclusive => "skipped",
        ChecksState::Success => "success",
        ChecksState::Failure => "failure",
    };
    Some(vec![CheckResult {
        kind: CheckResultKind::CommitStatus,
        name: "fake/ci".into(),
        status: status.into(),
        conclusion: None,
    }])
}

impl FakeForge {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            state: Arc::new(Mutex::new(State {
                next_number: 1,
                ..Default::default()
            })),
        }
    }

    pub fn repo_path(&self, remote: &str) -> PathBuf {
        self.root.join(format!("{remote}.git"))
    }

    /// Creates the bare repository for `remote` if missing.
    pub fn ensure_repository(&self, remote: &str) -> Result<Repository> {
        let path = self.repo_path(remote);
        if path.exists() {
            return Ok(Repository::open_bare(&path)?);
        }
        std::fs::create_dir_all(&path).map_err(|e| ForgeError::Other(e.to_string()))?;
        Ok(Repository::init_bare(&path)?)
    }

    fn open(&self, remote: &str) -> Result<Repository> {
        Repository::open_bare(self.repo_path(remote)).map_err(|_| ForgeError::NotFound {
            entity: "repository",
            id: remote.to_owned(),
        })
    }

    /// Lets a test decide what checks a merge request reports.
    pub fn set_checks(&self, remote: &str, source_branch: &str, state: ChecksState) {
        self.state
            .lock()
            .unwrap()
            .checks
            .insert((remote.to_owned(), source_branch.to_owned()), state);
    }

    /// Simulates a forge outage after the branch push but before a merge request is created.
    pub fn fail_next_create(&self, remote: &str) {
        self.state
            .lock()
            .unwrap()
            .fail_next_create
            .insert(remote.to_owned());
    }

    /// Simulates a human or another bot opening a merge request (for example Renovate).
    pub fn open_merge_request_as(
        &self,
        remote: &str,
        author: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        self.create_as(remote, author, request)
    }

    fn create_as(
        &self,
        remote: &str,
        author: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        let repo = self.open(remote)?;
        let head = repo
            .refname_to_id(&format!("refs/heads/{}", request.source_branch))
            .map_err(|_| ForgeError::NotFound {
                entity: "branch",
                id: request.source_branch.clone(),
            })?;
        let mut state = self.state.lock().unwrap();
        let number = state.next_number;
        state.next_number += 1;
        let checks = state
            .checks
            .get(&(remote.to_owned(), request.source_branch.clone()))
            .copied()
            .unwrap_or(ChecksState::Pending);
        let info = MergeRequestInfo {
            number,
            url: format!("fake://{remote}/merge_requests/{number}"),
            title: request.title.clone(),
            body: request.body.clone(),
            author_login: author.to_owned(),
            source_branch: request.source_branch.clone(),
            target_branch: request.target_branch.clone(),
            head_commit: CommitSha(head.to_string()),
            merge_commit: None,
            state: MergeRequestState::Open,
            draft: request.draft,
            checks,
            check_results: check_results(checks),
        };
        state.merge_requests.insert(
            (remote.to_owned(), number),
            StoredMergeRequest { info: info.clone() },
        );
        Ok(info)
    }

    /// Refreshes the stored head and checks from the live repository state.
    fn refresh(
        &self,
        remote: &str,
        repo: &Repository,
        stored: &mut StoredMergeRequest,
        state: &State,
    ) {
        if stored.info.state != MergeRequestState::Open {
            return;
        }
        if let Ok(head) = repo.refname_to_id(&format!("refs/heads/{}", stored.info.source_branch)) {
            stored.info.head_commit = CommitSha(head.to_string());
        }
        if let Some(checks) = state
            .checks
            .get(&(remote.to_owned(), stored.info.source_branch.clone()))
        {
            stored.info.checks = *checks;
            stored.info.check_results = check_results(*checks);
        }
    }
}

impl ForgeAdapter for FakeForge {
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".github/workflows"]
    }
    fn git_push_workflow_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        self.git_push_credentials(remote)
    }
    fn proposals_for_branch(&self, remote: &str, branch: &str) -> Result<Vec<MergeRequestInfo>> {
        let numbers: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .merge_requests
            .iter()
            .filter(|((r, _), mr)| r == remote && mr.info.source_branch == branch)
            .map(|((_, n), _)| *n)
            .collect();
        numbers
            .into_iter()
            .map(|n| self.get_merge_request(remote, n))
            .collect()
    }
    fn list_reviews(&self, remote: &str, number: u64) -> Result<Vec<MergeRequestReview>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .reviews
            .get(&(remote.into(), number))
            .cloned()
            .unwrap_or_default())
    }
    fn create_comment_review(
        &self,
        remote: &str,
        number: u64,
        head: &CommitSha,
        body: &str,
    ) -> Result<MergeRequestReview> {
        if self.get_merge_request(remote, number)?.head_commit != *head {
            return Err(ForgeError::Other("source moved".into()));
        }
        let mut state = self.state.lock().unwrap();
        let reviews = state.reviews.entry((remote.into(), number)).or_default();
        let review = MergeRequestReview {
            state: Default::default(),
            id: reviews.len() as u64 + 1,
            author_login: "artidezi[bot]".into(),
            head_commit: head.clone(),
            body: body.into(),
        };
        reviews.push(review.clone());
        Ok(review)
    }

    fn writes_allowed(&self) -> bool {
        true
    }
    fn merge_request_ref(&self, remote: &str, number: u64) -> Result<String> {
        Ok(format!(
            "refs/heads/{}",
            self.get_merge_request(remote, number)?.source_branch
        ))
    }
    fn clone_url(&self, remote: &str) -> String {
        self.repo_path(remote).display().to_string()
    }

    fn git_credentials(&self, _remote: &str) -> Result<Option<HttpsCredentials>> {
        Ok(None)
    }

    fn branch_head(&self, remote: &str, branch: &str) -> Result<CommitSha> {
        let repo = self.open(remote)?;
        let oid = repo
            .refname_to_id(&format!("refs/heads/{branch}"))
            .map_err(|_| ForgeError::NotFound {
                entity: "branch",
                id: branch.to_owned(),
            })?;
        Ok(CommitSha(oid.to_string()))
    }

    fn create_merge_request(
        &self,
        remote: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        if self.state.lock().unwrap().fail_next_create.remove(remote) {
            return Err(ForgeError::Other(
                "simulated merge request creation failure".into(),
            ));
        }
        self.create_as(remote, "artidezi[bot]", request)
    }

    fn update_merge_request(
        &self,
        remote: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let stored = state
            .merge_requests
            .get_mut(&(remote.to_owned(), number))
            .ok_or_else(|| ForgeError::NotFound {
                entity: "merge request",
                id: number.to_string(),
            })?;
        stored.info.title = title.to_owned();
        stored.info.body = body.to_owned();
        Ok(())
    }

    fn get_merge_request(&self, remote: &str, number: u64) -> Result<MergeRequestInfo> {
        let repo = self.open(remote)?;
        let mut state = self.state.lock().unwrap();
        let snapshot_checks = State {
            checks: state.checks.clone(),
            ..Default::default()
        };
        let stored = state
            .merge_requests
            .get_mut(&(remote.to_owned(), number))
            .ok_or_else(|| ForgeError::NotFound {
                entity: "merge request",
                id: number.to_string(),
            })?;
        self.refresh(remote, &repo, stored, &snapshot_checks);
        Ok(stored.info.clone())
    }

    fn list_open_merge_requests(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestInfo>> {
        let repo = self.open(remote)?;
        let mut state = self.state.lock().unwrap();
        let snapshot_checks = State {
            checks: state.checks.clone(),
            ..Default::default()
        };
        let mut result = vec![];
        for ((r, _), stored) in state.merge_requests.iter_mut() {
            if r != remote || stored.info.state != MergeRequestState::Open {
                continue;
            }
            if let Some(author) = author
                && stored.info.author_login != author
            {
                continue;
            }
            self.refresh(remote, &repo, stored, &snapshot_checks);
            result.push(stored.info.clone());
        }
        Ok(result)
    }

    fn merge(
        &self,
        remote: &str,
        number: u64,
        expected_head: &CommitSha,
        method: MergeMethod,
    ) -> Result<CommitSha> {
        let info = self.get_merge_request(remote, number)?;
        if info.state != MergeRequestState::Open {
            return Err(ForgeError::NotMergeable(number, "not open".into()));
        }
        if &info.head_commit != expected_head {
            return Err(ForgeError::HeadMoved {
                expected: expected_head.clone(),
                actual: info.head_commit,
            });
        }
        let repo = self.open(remote)?;
        let target_ref = format!("refs/heads/{}", info.target_branch);
        let target = repo.refname_to_id(&target_ref)?;
        let head =
            Oid::from_str(&info.head_commit.0).map_err(|e| ForgeError::Other(e.to_string()))?;
        let result = if repo.graph_descendant_of(head, target)? || head == target {
            // Fast-forward. Both methods behave the same when the source is a descendant.
            repo.reference(&target_ref, head, true, "fake forge merge")?;
            head
        } else {
            match method {
                MergeMethod::Merge => {
                    let merge_base = repo.merge_base(target, head)?;
                    let ancestor = repo.find_commit(merge_base)?.tree()?;
                    let ours = repo.find_commit(target)?;
                    let theirs = repo.find_commit(head)?;
                    let mut index =
                        repo.merge_trees(&ancestor, &ours.tree()?, &theirs.tree()?, None)?;
                    if index.has_conflicts() {
                        return Err(ForgeError::NotMergeable(number, "conflicts".into()));
                    }
                    let tree = repo.find_tree(index.write_tree_to(&repo)?)?;
                    let sig = git2::Signature::now("fake forge", "forge@invalid")?;
                    repo.commit(
                        Some(&target_ref),
                        &sig,
                        &sig,
                        &format!("Merge request !{number}"),
                        &tree,
                        &[&ours, &theirs],
                    )?
                }
                MergeMethod::Rebase => {
                    return Err(ForgeError::NotMergeable(
                        number,
                        "fake forge does not rebase diverged branches".into(),
                    ));
                }
            }
        };
        let mut state = self.state.lock().unwrap();
        if let Some(stored) = state.merge_requests.get_mut(&(remote.to_owned(), number)) {
            stored.info.state = MergeRequestState::Merged;
            stored.info.merge_commit = Some(CommitSha(result.to_string()));
        }
        Ok(CommitSha(result.to_string()))
    }

    fn tag_commit(&self, remote: &str, name: &str) -> Result<Option<CommitSha>> {
        let tags = &self.state.lock().unwrap().tags;
        Ok(tags.get(&(remote.to_owned(), name.to_owned())).cloned())
    }

    fn create_tag(&self, remote: &str, name: &str, commit: &CommitSha) -> Result<()> {
        let repo = self.open(remote)?;
        let refname = format!("refs/tags/{name}");
        if repo.refname_to_id(&refname).is_ok() {
            return Err(ForgeError::Other(format!("tag {name} already exists")));
        }
        let oid = Oid::from_str(&commit.0).map_err(|e| ForgeError::Other(e.to_string()))?;
        repo.reference(&refname, oid, false, "fake forge tag")?;
        self.state
            .lock()
            .unwrap()
            .tags
            .insert((remote.to_owned(), name.to_owned()), commit.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &Repository, parent: Option<Oid>, file: &str, content: &str, msg: &str) -> Oid {
        let mut builder = match parent {
            Some(p) => repo
                .treebuilder(Some(&repo.find_commit(p).unwrap().tree().unwrap()))
                .unwrap(),
            None => repo.treebuilder(None).unwrap(),
        };
        let blob = repo.blob(content.as_bytes()).unwrap();
        builder.insert(file, blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@invalid").unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|p| repo.find_commit(p).unwrap())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &sig, &sig, msg, &tree, &parent_refs)
            .unwrap()
    }

    #[test]
    fn merge_request_lifecycle_with_real_branch_updates() {
        let dir = tempfile::tempdir().unwrap();
        let forge = FakeForge::new(dir.path());
        let repo = forge.ensure_repository("org/role").unwrap();
        let base = commit(&repo, None, "a", "1", "base");
        repo.reference("refs/heads/main", base, true, "").unwrap();
        let feature = commit(&repo, Some(base), "a", "2", "feature");
        repo.reference("refs/heads/renovate/x", feature, true, "")
            .unwrap();

        forge.set_checks("org/role", "renovate/x", ChecksState::Success);
        let mr = forge
            .open_merge_request_as(
                "org/role",
                "renovate[bot]",
                &NewMergeRequest {
                    title: "Update x".into(),
                    body: "".into(),
                    source_branch: "renovate/x".into(),
                    target_branch: "main".into(),
                    draft: false,
                },
            )
            .unwrap();
        assert_eq!(mr.checks, ChecksState::Success);
        assert_eq!(
            forge
                .list_open_merge_requests("org/role", Some("renovate[bot]"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            forge
                .list_open_merge_requests("org/role", Some("artidezi[bot]"))
                .unwrap()
                .is_empty()
        );

        let stale = CommitSha(base.to_string());
        assert!(matches!(
            forge.merge("org/role", mr.number, &stale, MergeMethod::Merge),
            Err(ForgeError::HeadMoved { .. })
        ));
        let merged = forge
            .merge("org/role", mr.number, &mr.head_commit, MergeMethod::Merge)
            .unwrap();
        assert_eq!(merged.0, feature.to_string());
        assert_eq!(
            forge.branch_head("org/role", "main").unwrap().0,
            feature.to_string()
        );
        assert_eq!(
            forge
                .get_merge_request("org/role", mr.number)
                .unwrap()
                .state,
            MergeRequestState::Merged
        );

        forge.create_tag("org/role", "v1", &merged).unwrap();
        assert!(forge.create_tag("org/role", "v1", &merged).is_err());
    }
}
