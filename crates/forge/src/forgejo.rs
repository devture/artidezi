// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Forgejo adapter over the Gitea-compatible REST API (`/api/v1`), authenticated with a personal
//! access token of the bot user. Also works against Gitea, which is not a goal but falls out of
//! the shared API.

use artidezi_contract::common::CommitSha;
use artidezi_git::cache::HttpsCredentials;
use serde::Deserialize;
use serde_json::json;

use crate::*;

pub struct ForgejoForge {
    /// Instance root, for example `https://code.example.org`. No trailing slash.
    base_url: String,
    token: String,
    bot_login: String,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct BranchResponse {
    commit: BranchCommit,
}

#[derive(Deserialize)]
struct BranchCommit {
    id: String,
}

#[derive(Deserialize)]
struct PullResponse {
    number: u64,
    html_url: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    user: UserRef,
    head: RefInfo,
    base: RefInfo,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
struct UserRef {
    login: String,
}

#[derive(Deserialize)]
struct RefInfo {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Deserialize)]
struct CombinedStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    statuses: Option<Vec<serde_json::Value>>,
}

impl ForgejoForge {
    pub fn new(base_url: &str, token: &str, bot_login: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            bot_login: bot_login.to_owned(),
            agent,
        }
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base_url)
    }

    fn auth(&self) -> String {
        format!("token {}", self.token)
    }

    fn check(&self, status: u16, body: &str, what: &str) -> Result<()> {
        match status {
            200..=299 => Ok(()),
            404 => Err(ForgeError::NotFound {
                entity: "resource",
                id: what.to_owned(),
            }),
            _ => Err(ForgeError::Other(format!(
                "{what}: HTTP {status}: {}",
                body.trim()
            ))),
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut response = self
            .agent
            .get(self.api(path))
            .header("Authorization", &self.auth())
            .call()
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        self.check(status, &body, path)?;
        serde_json::from_str(&body).map_err(|e| ForgeError::Other(format!("{path}: bad JSON: {e}")))
    }

    fn send_json(&self, method: &str, path: &str, payload: &serde_json::Value) -> Result<String> {
        let request = match method {
            "POST" => self.agent.post(self.api(path)),
            "PATCH" => self.agent.patch(self.api(path)),
            _ => unreachable!("unsupported method"),
        };
        let mut response = request
            .header("Authorization", &self.auth())
            .send_json(payload)
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        self.check(status, &body, path)?;
        Ok(body)
    }

    fn split(remote: &str) -> Result<(&str, &str)> {
        remote
            .split_once('/')
            .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
            .ok_or_else(|| ForgeError::Other(format!("remote {remote:?} is not owner/name")))
    }

    fn checks_for(&self, owner: &str, repo: &str, sha: &str) -> Result<ChecksState> {
        let combined: CombinedStatus =
            self.get_json(&format!("/repos/{owner}/{repo}/commits/{sha}/status"))?;
        if combined.statuses.as_ref().is_none_or(Vec::is_empty) {
            return Ok(ChecksState::Absent);
        }
        Ok(match combined.state.as_str() {
            "success" => ChecksState::Success,
            "failure" | "error" => ChecksState::Failure,
            _ => ChecksState::Pending,
        })
    }

    fn info(&self, owner: &str, repo: &str, pull: PullResponse) -> Result<MergeRequestInfo> {
        let state = if pull.merged {
            MergeRequestState::Merged
        } else if pull.state == "open" {
            MergeRequestState::Open
        } else {
            MergeRequestState::Closed
        };
        let checks = if state == MergeRequestState::Open {
            self.checks_for(owner, repo, &pull.head.sha)?
        } else {
            ChecksState::Pending
        };
        Ok(MergeRequestInfo {
            number: pull.number,
            url: pull.html_url,
            title: pull.title,
            body: pull.body.unwrap_or_default(),
            author_login: pull.user.login,
            source_branch: pull.head.name,
            target_branch: pull.base.name,
            head_commit: CommitSha(pull.head.sha),
            merge_commit: pull.merge_commit_sha.map(CommitSha),
            state,
            draft: pull.draft,
            checks,
            check_results: None,
        })
    }

    /// Creates a private, initialized repository for the token's user. Test tooling only.
    pub fn create_repository(&self, name: &str, default_branch: &str) -> Result<String> {
        self.send_json(
            "POST",
            "/user/repos",
            &json!({"name": name, "private": true, "auto_init": true, "default_branch": default_branch}),
        )?;
        Ok(format!("{}/{name}", self.bot_login))
    }

    /// Posts a commit status. Test tooling only; real checks come from CI.
    pub fn set_commit_status(
        &self,
        remote: &str,
        sha: &str,
        state: &str,
        context: &str,
    ) -> Result<()> {
        let (owner, repo) = Self::split(remote)?;
        self.send_json(
            "POST",
            &format!("/repos/{owner}/{repo}/statuses/{sha}"),
            &json!({"state": state, "context": context, "description": "artidezi test"}),
        )?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct ReviewResponse {
    id: u64,
    user: UserRef,
    commit_id: String,
    #[serde(default)]
    body: String,
    state: String,
}
impl ReviewResponse {
    fn into_review(self) -> MergeRequestReview {
        MergeRequestReview {
            state: Default::default(),
            id: self.id,
            author_login: self.user.login,
            head_commit: CommitSha(self.commit_id),
            body: self.body,
        }
    }
}

impl ForgeAdapter for ForgejoForge {
    fn protected_paths(&self) -> &'static [&'static str] {
        &[
            ".forgejo/workflows",
            ".gitea/workflows",
            ".github/workflows",
        ]
    }
    /// The token has no narrower workflow-free scope to fall back from.
    fn git_push_workflow_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        self.git_push_credentials(remote)
    }
    fn proposals_for_branch(&self, remote: &str, branch: &str) -> Result<Vec<MergeRequestInfo>> {
        let mut result = vec![];
        let mut seen = std::collections::BTreeSet::new();
        for page in 1..=10 {
            let values: Vec<PullResponse> = self.get_json(&format!(
                "/repos/{remote}/pulls?state=all&limit=100&page={page}"
            ))?;
            let count = values.len();
            for value in values {
                if !seen.insert(value.number) {
                    return Err(ForgeError::Other("inconsistent proposal listing".into()));
                }
                if value.head.name == branch {
                    result.push(self.get_merge_request(remote, value.number)?);
                }
            }
            if count < 100 {
                return Ok(result);
            }
        }
        Err(ForgeError::Other(
            "proposal listing exceeds 1000 entries".into(),
        ))
    }
    fn list_reviews(&self, remote: &str, number: u64) -> Result<Vec<MergeRequestReview>> {
        let mut result = vec![];
        let mut seen = std::collections::BTreeSet::new();
        for page in 1..=10 {
            let values: Vec<ReviewResponse> = self.get_json(&format!(
                "/repos/{remote}/pulls/{number}/reviews?limit=100&page={page}"
            ))?;
            let count = values.len();
            for value in values {
                if !seen.insert(value.id) {
                    return Err(ForgeError::Other("inconsistent review listing".into()));
                }
                if value.state == "COMMENT" {
                    result.push(value.into_review());
                }
            }
            if count < 100 {
                return Ok(result);
            }
        }
        Err(ForgeError::Other(
            "review listing exceeds 1000 entries".into(),
        ))
    }
    fn create_comment_review(
        &self,
        remote: &str,
        number: u64,
        head: &CommitSha,
        body: &str,
    ) -> Result<MergeRequestReview> {
        let current = self.get_merge_request(remote, number)?;
        if current.head_commit != *head || current.state != MergeRequestState::Open || current.draft
        {
            return Err(ForgeError::Other("source changed before review".into()));
        }
        let response: ReviewResponse = serde_json::from_str(&self.send_json(
            "POST",
            &format!("/repos/{remote}/pulls/{number}/reviews"),
            &json!({"event":"COMMENT","commit_id":head,"body":body}),
        )?)
        .map_err(|e| ForgeError::Other(e.to_string()))?;
        if response.user.login != self.bot_login
            || response.commit_id != head.as_str()
            || response.body != body
            || response.state != "COMMENT"
        {
            return Err(ForgeError::Other("unexpected review receipt".into()));
        }
        Ok(response.into_review())
    }

    fn writes_allowed(&self) -> bool {
        true
    }
    fn merge_request_ref(&self, _remote: &str, number: u64) -> Result<String> {
        Ok(format!("refs/pull/{number}/head"))
    }
    fn clone_url(&self, remote: &str) -> String {
        format!("{}/{remote}.git", self.base_url)
    }

    fn git_credentials(&self, _remote: &str) -> Result<Option<HttpsCredentials>> {
        Ok(Some(HttpsCredentials {
            username: self.bot_login.clone(),
            token: self.token.clone(),
        }))
    }

    fn branch_head(&self, remote: &str, branch: &str) -> Result<CommitSha> {
        let (owner, repo) = Self::split(remote)?;
        let response: BranchResponse =
            self.get_json(&format!("/repos/{owner}/{repo}/branches/{branch}"))?;
        Ok(CommitSha(response.commit.id))
    }

    fn create_merge_request(
        &self,
        remote: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        let (owner, repo) = Self::split(remote)?;
        // Forgejo derives draft state from a WIP title prefix.
        let title = if request.draft {
            format!("WIP: {}", request.title)
        } else {
            request.title.clone()
        };
        let body = self.send_json(
            "POST",
            &format!("/repos/{owner}/{repo}/pulls"),
            &json!({"title": title, "body": request.body, "head": request.source_branch, "base": request.target_branch}),
        )?;
        let pull: PullResponse =
            serde_json::from_str(&body).map_err(|e| ForgeError::Other(e.to_string()))?;
        self.info(owner, repo, pull)
    }

    fn update_merge_request(
        &self,
        remote: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let (owner, repo) = Self::split(remote)?;
        self.send_json(
            "PATCH",
            &format!("/repos/{owner}/{repo}/pulls/{number}"),
            &json!({"title": title, "body": body}),
        )?;
        Ok(())
    }

    fn close_merge_request(
        &self,
        remote: &str,
        expected: &ExpectedProposal<'_>,
        comment: &str,
    ) -> Result<()> {
        let (owner, repo) = Self::split(remote)?;
        let number = expected.number;
        // Forgejo has no conditional close, so the proposal is rechecked right before each write.
        let require_expected = || {
            let current = self.get_merge_request(remote, number)?;
            if expected.matches(&current, &self.bot_login) {
                Ok(())
            } else {
                Err(ForgeError::Other(
                    "proposal is foreign, closed or changed; refusing to close it".into(),
                ))
            }
        };
        require_expected()?;
        self.send_json(
            "POST",
            &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            &json!({ "body": comment }),
        )?;
        require_expected()?;
        let pull: PullResponse = serde_json::from_str(&self.send_json(
            "PATCH",
            &format!("/repos/{owner}/{repo}/pulls/{number}"),
            &json!({"state": "closed"}),
        )?)
        .map_err(|e| ForgeError::Other(e.to_string()))?;
        if pull.number == number && !pull.merged && pull.head.sha != expected.head.as_str() {
            // A push landed after the last check, so the close hit a newer head that is not ours to close.
            let reopened = self
                .send_json(
                    "PATCH",
                    &format!("/repos/{owner}/{repo}/pulls/{number}"),
                    &json!({"state": "open"}),
                )
                .is_ok()
                && self
                    .get_merge_request(remote, number)
                    .is_ok_and(|pull| pull.state == MergeRequestState::Open);
            return Err(ForgeError::Other(if reopened {
                "proposal moved while it was being closed; reopened it for human review".into()
            } else {
                "proposal moved while it was being closed and could not be reopened; reopen it by hand".into()
            }));
        }
        if pull.number != number || pull.state != "closed" || pull.merged {
            return Err(ForgeError::Other(
                "forge did not confirm closing the expected proposal".into(),
            ));
        }
        Ok(())
    }

    fn get_merge_request(&self, remote: &str, number: u64) -> Result<MergeRequestInfo> {
        let (owner, repo) = Self::split(remote)?;
        let pull: PullResponse = self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"))?;
        self.info(owner, repo, pull)
    }

    fn list_open_merge_requests(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestInfo>> {
        let (owner, repo) = Self::split(remote)?;
        let mut result = vec![];
        for page in 1..=20u32 {
            let pulls: Vec<PullResponse> = self.get_json(&format!(
                "/repos/{owner}/{repo}/pulls?state=open&limit=50&page={page}"
            ))?;
            let done = pulls.len() < 50;
            for pull in pulls {
                if let Some(author) = author
                    && pull.user.login != author
                {
                    continue;
                }
                result.push(self.info(owner, repo, pull)?);
            }
            if done {
                return Ok(result);
            }
        }
        Err(ForgeError::Other(
            "open PR listing exceeds the 1,000-PR limit; refusing an incomplete discovery scan"
                .into(),
        ))
    }

    fn merge(
        &self,
        remote: &str,
        number: u64,
        expected_head: &CommitSha,
        method: MergeMethod,
    ) -> Result<CommitSha> {
        let (owner, repo) = Self::split(remote)?;
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
        let do_what = match method {
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        };
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/merge");
        let mut response = self
            .agent
            .post(self.api(&path))
            .header("Authorization", &self.auth())
            .send_json(json!({"Do": do_what, "head_commit_id": expected_head.as_str(), "delete_branch_after_merge": false}))
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        match status {
            200..=299 => {}
            405 | 409 => return Err(ForgeError::NotMergeable(number, body.trim().to_owned())),
            _ => self.check(status, &body, &path)?,
        }
        self.branch_head(remote, &info.target_branch)
    }

    fn tag_commit(&self, remote: &str, name: &str) -> Result<Option<CommitSha>> {
        #[derive(Deserialize)]
        struct Tag {
            commit: TagCommit,
        }
        #[derive(Deserialize)]
        struct TagCommit {
            sha: String,
        }
        let (owner, repo) = Self::split(remote)?;
        match self.get_json::<Tag>(&format!("/repos/{owner}/{repo}/tags/{name}")) {
            Ok(tag) => Ok(Some(CommitSha(tag.commit.sha))),
            Err(ForgeError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn create_tag(&self, remote: &str, name: &str, commit: &CommitSha) -> Result<()> {
        let (owner, repo) = Self::split(remote)?;
        let path = format!("/repos/{owner}/{repo}/tags");
        let mut response = self
            .agent
            .post(self.api(&path))
            .header("Authorization", &self.auth())
            .send_json(json!({"tag_name": name, "target": commit.as_str()}))
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ForgeError::Other(e.to_string()))?;
        if status == 409 {
            return Err(ForgeError::Other(format!("tag {name} already exists")));
        }
        self.check(status, &body, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    struct Stub {
        head: String,
        state: &'static str,
        push_before_close: Option<String>,
        fail_reopen: bool,
        comments: usize,
    }

    fn pull(stub: &Stub) -> Value {
        json!({"number":5,"html_url":"http://forge.invalid/o/r/pulls/5","title":"Adapt","body":"","user":{"login":"artidezi"},
            "head":{"ref":"artidezi/task","sha":stub.head},"base":{"ref":"main","sha":"b".repeat(40)},"state":stub.state,"merged":false})
    }

    /// Loopback Forgejo-shaped API for the proposal-close race; one request per connection.
    fn serve(stub: Arc<Mutex<Stub>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap().to_owned(),
                    parts.next().unwrap().to_owned(),
                );
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut stub = stub.lock().unwrap();
                let (status, reply) = match (method.as_str(), path.as_str()) {
                    ("GET", "/api/v1/repos/o/r/pulls/5") => (200, pull(&stub)),
                    ("GET", p) if p.ends_with("/status") => {
                        (200, json!({"state":"","statuses":[]}))
                    }
                    ("POST", "/api/v1/repos/o/r/issues/5/comments") => {
                        stub.comments += 1;
                        (201, json!({}))
                    }
                    ("PATCH", "/api/v1/repos/o/r/pulls/5") if body["state"] == "closed" => {
                        if let Some(pushed) = stub.push_before_close.take() {
                            stub.head = pushed;
                        }
                        stub.state = "closed";
                        (201, pull(&stub))
                    }
                    ("PATCH", "/api/v1/repos/o/r/pulls/5") if stub.fail_reopen => {
                        (422, json!({"message":"cannot reopen"}))
                    }
                    ("PATCH", "/api/v1/repos/o/r/pulls/5") => {
                        stub.state = "open";
                        (201, pull(&stub))
                    }
                    _ => panic!("unexpected stub request: {method} {path}"),
                };
                let reply = reply.to_string();
                write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        });
        url
    }

    #[test]
    fn a_push_racing_the_close_reopens_the_proposal_at_the_new_head() {
        for fail_reopen in [false, true] {
            let head = CommitSha("c".repeat(40));
            let pushed = "d".repeat(40);
            let stub = Arc::new(Mutex::new(Stub {
                head: head.0.clone(),
                state: "open",
                push_before_close: Some(pushed.clone()),
                fail_reopen,
                comments: 0,
            }));
            let forge = ForgejoForge::new(&serve(stub.clone()), "token", "artidezi");
            let expected = ExpectedProposal {
                number: 5,
                source_branch: "artidezi/task",
                target_branch: "main",
                head: &head,
            };
            let error = forge
                .close_merge_request("o/r", &expected, "Superseded.")
                .unwrap_err()
                .to_string();
            let stub = stub.lock().unwrap();
            assert_eq!(stub.head, pushed);
            assert_eq!(stub.comments, 1);
            if fail_reopen {
                assert!(error.contains("could not be reopened"), "{error}");
                assert_eq!(stub.state, "closed");
            } else {
                assert!(error.contains("reopened it"), "{error}");
                assert_eq!(stub.state, "open");
            }
        }
    }
}
