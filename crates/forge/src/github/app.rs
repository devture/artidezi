// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! GitHub App authentication, narrow read tokens and opt-in publication credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use openssl::{
    hash::MessageDigest,
    pkey::{Id, PKey, Private},
    sign::Signer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::http::Http;
use super::{
    GithubReader, PullRef, PullRequest, Repository, validate_pull_identity, validate_remote,
    validate_source,
};
use crate::{
    CheckResult, CheckResultKind, ChecksState, CiJobLog, ExpectedProposal, ForgeAdapter,
    ForgeError, MergeMethod, MergeRequestInfo, MergeRequestListing, MergeRequestState,
    MergeRequestSummary, NewMergeRequest, Result, SourceActionsWithoutChecks, SourceMergeAuthors,
    source_checks_permit,
};
use artidezi_contract::common::CommitSha;
use artidezi_git::cache::HttpsCredentials;

const READ_PERMISSIONS: [&str; 5] = [
    "metadata",
    "contents",
    "pull_requests",
    "checks",
    "statuses",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GithubAppIdentity {
    pub client_id: String,
    pub installation_id: u64,
    pub account: String,
    pub bot_login: String,
}

#[derive(Deserialize)]
struct App {
    id: u64,
    client_id: String,
    slug: String,
}
#[derive(Deserialize)]
struct Account {
    login: String,
}
#[derive(Deserialize)]
struct Installation {
    id: u64,
    app_id: u64,
    account: Account,
    repository_selection: String,
    permissions: BTreeMap<String, String>,
    suspended_at: Option<String>,
}
#[derive(Deserialize)]
struct TokenRepository {
    full_name: String,
}
#[derive(Deserialize)]
struct TokenReply {
    token: String,
    expires_at: DateTime<Utc>,
    permissions: BTreeMap<String, String>,
    repositories: Vec<TokenRepository>,
}
struct Token {
    value: String,
    expires_at: DateTime<Utc>,
}

/// No arbitrary endpoints, personal credentials or write capability. Each token is restricted to
/// one configured repository and read permissions, even if the App installation has broader access.
pub struct GithubForge {
    identity: GithubAppIdentity,
    key: PKey<Private>,
    repositories: BTreeSet<String>,
    tokens: Mutex<BTreeMap<String, Token>>,
    actions_tokens: Mutex<BTreeMap<String, Token>>,
    publication_repositories: BTreeSet<String>,
    approval_repositories: BTreeSet<String>,
    /// Authors per repository whose pull requests a merge handler may merge.
    merge_authors: SourceMergeAuthors,
    /// Narrower still: authors whose pull requests may be approved or merged with no checks.
    without_checks: SourceActionsWithoutChecks,
    /// Lowercased repositories where ArtiDezi may merge its own adaptation pull requests.
    adaptation_merge_repositories: BTreeSet<String>,
    write_tokens: Mutex<BTreeMap<String, Token>>,
    workflow_write_tokens: Mutex<BTreeMap<String, Token>>,
    http: Arc<Http>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenScope {
    Read,
    ActionsRead,
    Publication,
    WorkflowPublication,
}

impl GithubForge {
    pub fn new(
        identity: GithubAppIdentity,
        pem: &str,
        repositories: BTreeSet<String>,
    ) -> Result<Self> {
        Self::at(identity, pem, repositories, "https://api.github.com")
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn at_for_fixture(
        identity: GithubAppIdentity,
        pem: &str,
        repositories: BTreeSet<String>,
        api: &str,
    ) -> Result<Self> {
        let port = api
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok());
        if port.is_none_or(|p| p == 0) {
            return Err(invalid("test API must be loopback"));
        }
        Self::at(identity, pem, repositories, api)
    }

    fn at(
        identity: GithubAppIdentity,
        pem: &str,
        repositories: BTreeSet<String>,
        api: &str,
    ) -> Result<Self> {
        if identity.installation_id == 0
            || identity.client_id.is_empty()
            || !identity
                .client_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            || identity.account.is_empty()
            || !identity
                .account
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || !identity.bot_login.ends_with("[bot]")
            || repositories.is_empty()
        {
            return Err(invalid(
                "GitHub needs an App client ID, installation ID/account, bot login and explicit repositories",
            ));
        }
        let repositories: BTreeSet<_> =
            repositories.into_iter().map(|r| r.to_lowercase()).collect();
        for remote in &repositories {
            validate_remote(remote)?;
            if !remote
                .split_once('/')
                .is_some_and(|(account, _)| account.eq_ignore_ascii_case(&identity.account))
            {
                return Err(invalid(
                    "GitHub repository is outside the configured installation account",
                ));
            }
        }
        // Avoid OpenSSL's interactive decryption callback; service credentials must be unattended.
        if pem.contains("ENCRYPTED") {
            return Err(invalid(
                "GitHub App key must be an unencrypted RSA PEM file protected by filesystem permissions",
            ));
        }
        let key = PKey::private_key_from_pem(pem.as_bytes())
            .map_err(|_| invalid("GitHub App private key is not a valid RSA PEM"))?;
        if key.id() != Id::RSA || key.bits() < 2048 {
            return Err(invalid(
                "GitHub App requires an RSA key of at least 2048 bits",
            ));
        }
        Ok(Self {
            identity,
            key,
            repositories,
            tokens: Mutex::new(BTreeMap::new()),
            actions_tokens: Mutex::new(BTreeMap::new()),
            publication_repositories: BTreeSet::new(),
            approval_repositories: BTreeSet::new(),
            merge_authors: SourceMergeAuthors::default(),
            without_checks: SourceActionsWithoutChecks::default(),
            adaptation_merge_repositories: BTreeSet::new(),
            write_tokens: Mutex::new(BTreeMap::new()),
            workflow_write_tokens: Mutex::new(BTreeMap::new()),
            http: Arc::new(Http::new(api)),
        })
    }

    /// Explicit controller configuration, separate from the App installation's permissions.
    pub fn with_publication_repositories(mut self, repositories: BTreeSet<String>) -> Result<Self> {
        for remote in &repositories {
            self.remote(remote)?;
        }
        self.publication_repositories =
            repositories.into_iter().map(|r| r.to_lowercase()).collect();
        Ok(self)
    }

    /// Approval and merge authority are narrower than ordinary publication authority. Merges are
    /// limited to the authors merge handlers name per repository.
    pub fn with_source_actions(
        mut self,
        approvals: BTreeSet<String>,
        merges: SourceMergeAuthors,
    ) -> Result<Self> {
        for remote in approvals.iter().map(String::as_str).chain(merges.remotes()) {
            self.require_publication(remote)?;
        }
        self.approval_repositories = approvals.into_iter().map(|r| r.to_lowercase()).collect();
        self.merge_authors = merges;
        Ok(self)
    }

    /// Source approvals and merges that may proceed when GitHub reports no checks at all for the
    /// head. Each still needs the matching `with_source_actions` authority.
    pub fn with_source_actions_without_checks(
        mut self,
        actions: SourceActionsWithoutChecks,
    ) -> Result<Self> {
        for remote in actions.approvals.remotes().chain(actions.merges.remotes()) {
            self.require_publication(remote)?;
        }
        self.without_checks = actions;
        Ok(self)
    }

    /// Repositories where ArtiDezi may merge its own `artidezi/` adaptation pull requests once
    /// their checks pass. Without it, the adapter never merges a pull request it opened.
    pub fn with_adaptation_merges(mut self, repositories: BTreeSet<String>) -> Result<Self> {
        for remote in &repositories {
            self.require_publication(remote)?;
        }
        self.adaptation_merge_repositories =
            repositories.into_iter().map(|r| r.to_lowercase()).collect();
        Ok(self)
    }

    fn require_publication(&self, remote: &str) -> Result<()> {
        self.remote(remote)?;
        if !self
            .publication_repositories
            .contains(&remote.to_lowercase())
        {
            return Err(read_only());
        }
        Ok(())
    }

    fn remote(&self, remote: &str) -> Result<()> {
        validate_remote(remote)?;
        if !self.repositories.contains(&remote.to_lowercase()) {
            return Err(invalid(
                "GitHub repository is outside this forge binding's configured scope",
            ));
        }
        Ok(())
    }

    fn jwt(&self) -> Result<String> {
        let now = Utc::now().timestamp();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({"iss": self.identity.client_id, "iat": now - 60, "exp": now + 540}).to_string(),
        );
        let payload = format!("{header}.{claims}");
        let mut signer = Signer::new(MessageDigest::sha256(), &self.key)
            .map_err(|_| invalid("GitHub App JWT signer initialization failed"))?;
        signer
            .update(payload.as_bytes())
            .map_err(|_| invalid("GitHub App JWT signing failed"))?;
        let signature = signer
            .sign_to_vec()
            .map_err(|_| invalid("GitHub App JWT signing failed"))?;
        Ok(format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    fn token(&self, remote: &str) -> Result<String> {
        self.scoped_token(remote, TokenScope::Read)
    }

    pub(super) fn actions_token(&self, remote: &str) -> Result<String> {
        self.scoped_token(remote, TokenScope::ActionsRead)
    }

    pub(super) fn http(&self) -> &Http {
        &self.http
    }

    fn scoped_token(&self, remote: &str, scope: TokenScope) -> Result<String> {
        self.remote(remote)?;
        if matches!(
            scope,
            TokenScope::Publication | TokenScope::WorkflowPublication
        ) {
            self.require_publication(remote)?;
        }
        let remote = remote.to_lowercase();
        let mut tokens = match scope {
            TokenScope::Read => self.tokens.lock().unwrap(),
            TokenScope::ActionsRead => self.actions_tokens.lock().unwrap(),
            TokenScope::Publication => self.write_tokens.lock().unwrap(),
            TokenScope::WorkflowPublication => self.workflow_write_tokens.lock().unwrap(),
        };
        if let Some(token) = tokens.get(&remote)
            && token.expires_at > Utc::now() + Duration::seconds(120)
        {
            return Ok(token.value.clone());
        }
        // Recheck installation identity/permission changes whenever a repository token is renewed.
        let jwt = self.jwt()?;
        let app: App = self.http.get("/app", Some(&jwt), false)?;
        if app.client_id != self.identity.client_id
            || format!("{}[bot]", app.slug) != self.identity.bot_login
        {
            return Err(invalid(
                "GitHub App identity does not match client_id/bot_login",
            ));
        }
        let installation: Installation = self.http.get(
            &format!("/app/installations/{}", self.identity.installation_id),
            Some(&jwt),
            false,
        )?;
        if installation.id != self.identity.installation_id
            || installation.app_id != app.id
            || !installation
                .account
                .login
                .eq_ignore_ascii_case(&self.identity.account)
            || installation.suspended_at.is_some()
            || !matches!(
                installation.repository_selection.as_str(),
                "selected" | "all"
            )
        {
            return Err(invalid(
                "GitHub installation identity/selection is invalid or the installation is suspended",
            ));
        }
        // Installation authority may be broader; each issued token must exactly match this operation.
        let mut permissions: BTreeMap<String, String> = if scope == TokenScope::ActionsRead {
            [
                ("metadata".into(), "read".into()),
                ("actions".into(), "read".into()),
            ]
            .into()
        } else if scope == TokenScope::WorkflowPublication {
            [
                ("metadata".into(), "read".into()),
                ("contents".into(), "write".into()),
                ("workflows".into(), "write".into()),
            ]
            .into()
        } else {
            READ_PERMISSIONS
                .iter()
                .map(|p| (p.to_string(), "read".into()))
                .collect()
        };
        if scope == TokenScope::Publication {
            permissions.insert("contents".into(), "write".into());
            permissions.insert("pull_requests".into(), "write".into());
        }
        for (name, required) in &permissions {
            let actual = installation.permissions.get(name).map(String::as_str);
            if actual != Some(required.as_str()) && !(required == "read" && actual == Some("write"))
            {
                return Err(invalid(
                    "GitHub installation lacks required repository permissions",
                ));
            }
        }
        // Even an installation covering all repositories only receives credentials for this
        // explicitly configured repository. Installation scope never enrolls projects implicitly.
        let (_, name) = remote.split_once('/').expect("validated remote");
        let reply: TokenReply = self.http.installation_token(
            self.identity.installation_id,
            &jwt,
            &json!({"repositories": [name], "permissions": permissions}),
        )?;
        if reply.permissions != permissions {
            return Err(invalid(
                "GitHub token permissions differ from requested scope",
            ));
        }
        if reply.repositories.len() != 1
            || !reply.repositories[0]
                .full_name
                .eq_ignore_ascii_case(&remote)
            || reply.permissions.keys().any(|k| {
                !READ_PERMISSIONS.contains(&k.as_str())
                    && !(scope == TokenScope::ActionsRead && k == "actions")
                    && !(scope == TokenScope::WorkflowPublication && k == "workflows")
            })
            || reply.token.is_empty()
            || reply.token.chars().any(char::is_whitespace)
            || reply.expires_at <= Utc::now() + Duration::seconds(120)
            || reply.expires_at > Utc::now() + Duration::seconds(3900)
        {
            return Err(invalid(
                "GitHub installation token returned unexpected repository/permission scope or expiration",
            ));
        }
        self.http.clear_cache();
        let value = reply.token;
        tokens.insert(
            remote,
            Token {
                value: value.clone(),
                expires_at: reply.expires_at,
            },
        );
        Ok(value)
    }

    fn read<T>(&self, remote: &str, f: impl FnOnce(&GithubReader) -> Result<T>) -> Result<T> {
        let reader = GithubReader {
            token: Some(self.token(remote)?),
            http: self.http.clone(),
            conditional: true,
        };
        let result = (|| {
            let repository: Repository = reader.get(&format!("/repos/{remote}"))?;
            if !repository.full_name.eq_ignore_ascii_case(remote)
                || repository.archived
                || repository.disabled
            {
                return Err(invalid(
                    "GitHub repository identity changed or the repository is archived/disabled",
                ));
            }
            f(&reader)
        })();
        if matches!(result, Err(ForgeError::AuthenticationRejected)) {
            self.tokens.lock().unwrap().remove(&remote.to_lowercase());
            self.http.clear_cache();
        }
        result
    }

    fn mutate<T: serde::de::DeserializeOwned>(
        &self,
        remote: &str,
        method: &str,
        path: &str,
        body: &serde_json::Value,
        expected: u16,
    ) -> Result<T> {
        let token = self.scoped_token(remote, TokenScope::Publication)?;
        let result = self.http.mutate(method, path, &token, body, expected);
        if matches!(result, Err(ForgeError::AuthenticationRejected)) {
            self.write_tokens
                .lock()
                .unwrap()
                .remove(&remote.to_lowercase());
            self.http.clear_cache();
        }
        result
    }

    /// Live preflight: verify auth and selected repositories without scheduling a task or publishing.
    /// Installation-token issuance is the only POST performed.
    pub fn verify_installation(&self) -> Result<Vec<String>> {
        for remote in &self.repositories {
            self.read(remote, |_| Ok(()))?;
            if self.publication_repositories.contains(remote) {
                self.scoped_token(remote, TokenScope::Publication)?;
            }
        }
        Ok(self.repositories.iter().cloned().collect())
    }

    fn pull(&self, reader: &GithubReader, remote: &str, number: u64) -> Result<MergeRequestInfo> {
        if number == 0 {
            return Err(invalid("pull request number must be positive"));
        }
        let path = format!("/repos/{remote}/pulls/{number}");
        let source: PullRequest = reader.get(&path)?;
        validate_source(&source, remote, number)?;
        let (checks, check_results) = if source.state == "open" && !source.merged {
            let (statuses, check_runs, summary) = reader.checks(remote, &source.head.sha)?;
            let checks = match summary {
                "absent" => ChecksState::Absent,
                "inconclusive" => ChecksState::Inconclusive,
                "success" => ChecksState::Success,
                "failure" => ChecksState::Failure,
                _ => ChecksState::Pending,
            };
            let mut results = Vec::with_capacity(statuses.len() + check_runs.len());
            results.extend(statuses.into_iter().map(|status| CheckResult {
                kind: CheckResultKind::CommitStatus,
                name: status.context,
                status: status.state,
                conclusion: None,
            }));
            results.extend(check_runs.into_iter().map(|run| CheckResult {
                kind: CheckResultKind::CheckRun,
                name: run.name,
                status: run.status,
                conclusion: run.conclusion,
            }));
            (checks, Some(results))
        } else {
            (ChecksState::Pending, Some(Vec::new()))
        };
        let current: PullRequest = reader.get(&path)?;
        validate_source(&current, remote, number)?;
        if current.head.sha != source.head.sha
            || current.base.sha != source.base.sha
            || current.base.name != source.base.name
            || current.state != source.state
            || current.merged != source.merged
            || current.draft != source.draft
            || current.user.login != source.user.login
            || current.title != source.title
            || current.body != source.body
        {
            return Err(invalid("GitHub source changed while reading checks; retry"));
        }
        let state = match (source.merged, source.state.as_str()) {
            (true, _) => MergeRequestState::Merged,
            (false, "open") => MergeRequestState::Open,
            (false, "closed") => MergeRequestState::Closed,
            _ => return Err(invalid("GitHub returned an unknown source state")),
        };
        Ok(MergeRequestInfo {
            number,
            url: format!("https://github.com/{remote}/pull/{number}"),
            title: source.title,
            body: source.body.unwrap_or_default(),
            author_login: source.user.login,
            source_branch: source.head.name,
            target_branch: source.base.name,
            head_commit: CommitSha(source.head.sha),
            merge_commit: source.merge_commit_sha.map(CommitSha),
            state,
            draft: source.draft,
            checks,
            check_results,
        })
    }
}

impl ForgeAdapter for GithubForge {
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".github/workflows"]
    }
    fn ci_job_logs(&self, remote: &str, head: &CommitSha, names: &[String]) -> Vec<CiJobLog> {
        super::ci_logs::collect(self, remote, head, names)
    }
    fn writes_allowed(&self) -> bool {
        !self.publication_repositories.is_empty()
    }
    fn writes_allowed_for(&self, remote: &str) -> bool {
        self.publication_repositories
            .contains(&remote.to_lowercase())
    }
    fn git_push_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        Ok(Some(HttpsCredentials {
            username: "x-access-token".into(),
            token: self.scoped_token(remote, TokenScope::Publication)?,
        }))
    }
    fn git_push_workflow_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        Ok(Some(HttpsCredentials {
            username: "x-access-token".into(),
            token: self.scoped_token(remote, TokenScope::WorkflowPublication)?,
        }))
    }
    fn retry_after(&self) -> Option<DateTime<Utc>> {
        self.http.retry_after()
    }
    fn clone_url(&self, remote: &str) -> String {
        format!("https://github.com/{remote}.git")
    }
    fn git_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        Ok(Some(HttpsCredentials {
            username: "x-access-token".into(),
            token: self.token(remote)?,
        }))
    }
    fn branch_head(&self, remote: &str, branch: &str) -> Result<CommitSha> {
        if !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
            return Err(invalid("invalid GitHub target branch"));
        }
        self.read(remote, |reader| {
            #[derive(Deserialize)]
            struct Commit {
                sha: String,
            }
            #[derive(Deserialize)]
            struct Branch {
                name: String,
                commit: Commit,
            }
            let encoded: String = branch
                .bytes()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
                        (c as char).to_string()
                    } else {
                        format!("%{c:02X}")
                    }
                })
                .collect();
            let value: Branch = reader.get(&format!("/repos/{remote}/branches/{encoded}"))?;
            let sha = CommitSha(value.commit.sha);
            if value.name != branch || !sha.is_well_formed() {
                return Err(invalid("GitHub branch identity is invalid"));
            }
            Ok(sha)
        })
    }
    fn merge_request_ref(&self, remote: &str, number: u64) -> Result<String> {
        self.remote(remote)?;
        if number == 0 {
            return Err(invalid("pull request number must be positive"));
        }
        Ok(format!("refs/pull/{number}/head"))
    }
    fn get_merge_request(&self, remote: &str, number: u64) -> Result<MergeRequestInfo> {
        self.read(remote, |reader| self.pull(reader, remote, number))
    }
    fn list_open_merge_request_summaries(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestListing>> {
        // The token is bound to this configured repository. A full get_merge_request
        // validates repository state and the PR again before discovery prepares work.
        let reader = GithubReader {
            token: Some(self.token(remote)?),
            http: self.http.clone(),
            conditional: true,
        };
        let result = (|| {
            #[derive(Deserialize)]
            struct Listed {
                number: u64,
                title: String,
                user: super::Author,
                state: String,
                draft: bool,
                head: PullRef,
                base: PullRef,
            }
            let mut seen = BTreeSet::new();
            let mut summaries = Vec::new();
            for page in 1..=super::MAX_PAGES {
                let items: Vec<Listed> = reader.get(&format!(
                    "/repos/{remote}/pulls?state=open&sort=created&direction=asc&per_page=100&page={page}"
                ))?;
                let count = items.len();
                for item in items {
                    if item.number == 0 || !seen.insert(item.number) || item.state != "open" {
                        return Err(invalid(
                            "GitHub open-source listing changed during pagination",
                        ));
                    }
                    validate_pull_identity(
                        item.number,
                        &item.head,
                        &item.base,
                        remote,
                        item.number,
                    )?;
                    if author.is_none_or(|a| a.eq_ignore_ascii_case(&item.user.login)) {
                        summaries.push(MergeRequestListing {
                            summary: MergeRequestSummary {
                                number: item.number,
                                title: item.title,
                                author_login: item.user.login,
                                source_branch: item.head.name,
                                target_branch: item.base.name,
                                head_commit: CommitSha(item.head.sha),
                                draft: item.draft,
                            },
                            details: None,
                        });
                    }
                }
                if count < 100 {
                    return Ok(summaries);
                }
            }
            Err(invalid(
                "GitHub open-source listing exceeds 1000 entries; narrow enrollment",
            ))
        })();
        if matches!(result, Err(ForgeError::AuthenticationRejected)) {
            self.tokens.lock().unwrap().remove(&remote.to_lowercase());
            self.http.clear_cache();
        }
        result
    }
    fn list_open_merge_requests(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestInfo>> {
        self.read(remote, |reader| {
            #[derive(Deserialize)] struct Listed { number: u64, user: super::Author }
            let mut seen = BTreeSet::new();
            let mut numbers = Vec::new();
            for page in 1..=super::MAX_PAGES {
                let items: Vec<Listed> = reader.get(&format!("/repos/{remote}/pulls?state=open&sort=created&direction=asc&per_page=100&page={page}"))?;
                let count = items.len();
                for item in items {
                    if item.number == 0 || !seen.insert(item.number) { return Err(invalid("GitHub source listing changed during pagination")); }
                    if author.is_none_or(|a| a.eq_ignore_ascii_case(&item.user.login)) { numbers.push(item.number); }
                }
                if count < 100 { break; }
                if page == super::MAX_PAGES { return Err(invalid("GitHub open-source listing exceeds 1000 entries; narrow enrollment")); }
            }
            numbers.into_iter().map(|n| self.pull(reader, remote, n)).collect()
        })
    }
    fn proposals_for_branch(&self, remote: &str, branch: &str) -> Result<Vec<MergeRequestInfo>> {
        self.read(remote, |reader| {
            let owner = remote.split_once('/').unwrap().0;
            let head: String = format!("{owner}:{branch}")
                .bytes()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
                        (c as char).to_string()
                    } else {
                        format!("%{c:02X}")
                    }
                })
                .collect();
            let mut result = vec![];
            let mut seen = BTreeSet::new();
            for page in 1..=super::MAX_PAGES {
                #[derive(Deserialize)]
                struct Listed {
                    number: u64,
                }
                let values: Vec<Listed> = reader.get(&format!(
                    "/repos/{remote}/pulls?state=all&head={head}&per_page=100&page={page}"
                ))?;
                let count = values.len();
                for value in values {
                    if value.number == 0 || !seen.insert(value.number) {
                        return Err(invalid("inconsistent GitHub proposal listing"));
                    }
                    let pull = self.pull(reader, remote, value.number)?;
                    if pull.source_branch != branch {
                        return Err(invalid("GitHub proposal filter returned another branch"));
                    }
                    result.push(pull);
                }
                if count < 100 {
                    return Ok(result);
                }
            }
            Err(invalid("GitHub proposal listing exceeds 1000 entries"))
        })
    }
    fn create_merge_request(
        &self,
        remote: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        self.require_publication(remote)?;
        if !request.source_branch.starts_with("artidezi/")
            || !git2::Reference::is_valid_name(&format!("refs/heads/{}", request.source_branch))
        {
            return Err(invalid("GitHub proposals require an ArtiDezi-owned branch"));
        }
        let response: PullRequest = self.mutate(remote, "POST", &format!("/repos/{remote}/pulls"),
            &json!({"title":request.title,"body":request.body,"head":request.source_branch,"base":request.target_branch,"draft":request.draft}), 201)?;
        validate_source(&response, remote, response.number)?;
        if response.user.login != self.identity.bot_login
            || response.head.name != request.source_branch
            || response.base.name != request.target_branch
        {
            return Err(invalid(
                "GitHub returned a proposal with unexpected ownership",
            ));
        }
        self.get_merge_request(remote, response.number)
    }
    fn update_merge_request(
        &self,
        remote: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<()> {
        self.require_publication(remote)?;
        let current = self.get_merge_request(remote, number)?;
        if current.author_login != self.identity.bot_login
            || !current.source_branch.starts_with("artidezi/")
            || current.state != MergeRequestState::Open
        {
            return Err(invalid("cannot update a foreign or closed GitHub proposal"));
        }
        let _: serde_json::Value = self.mutate(
            remote,
            "PATCH",
            &format!("/repos/{remote}/pulls/{number}"),
            &json!({"title":title,"body":body}),
            200,
        )?;
        Ok(())
    }
    fn close_merge_request(
        &self,
        remote: &str,
        expected: &ExpectedProposal<'_>,
        comment: &str,
    ) -> Result<()> {
        self.require_publication(remote)?;
        let number = expected.number;
        // GitHub has no conditional close, so the proposal is rechecked right before each write.
        let require_expected = || {
            let current = self.get_merge_request(remote, number)?;
            if expected.matches(&current, &self.identity.bot_login) {
                Ok(())
            } else {
                Err(invalid(
                    "GitHub proposal is foreign, closed or changed; refusing to close it",
                ))
            }
        };
        require_expected()?;
        let _: serde_json::Value = self.mutate(
            remote,
            "POST",
            &format!("/repos/{remote}/issues/{number}/comments"),
            &json!({ "body": comment }),
            201,
        )?;
        require_expected()?;
        let response: PullRequest = self.mutate(
            remote,
            "PATCH",
            &format!("/repos/{remote}/pulls/{number}"),
            &json!({"state":"closed"}),
            200,
        )?;
        if response.number == number
            && !response.merged
            && response.head.sha != expected.head.as_str()
        {
            // A push landed after the last check, so the close hit a newer head that is not ours to close.
            let reopened = self
                .mutate::<PullRequest>(
                    remote,
                    "PATCH",
                    &format!("/repos/{remote}/pulls/{number}"),
                    &json!({"state":"open"}),
                    200,
                )
                .is_ok_and(|pull| pull.state == "open")
                && self
                    .get_merge_request(remote, number)
                    .is_ok_and(|pull| pull.state == MergeRequestState::Open);
            return Err(invalid(if reopened {
                "GitHub proposal moved while it was being closed; reopened it for human review"
            } else {
                "GitHub proposal moved while it was being closed and could not be reopened; reopen it by hand"
            }));
        }
        if response.number != number || response.state != "closed" || response.merged {
            return Err(invalid(
                "GitHub did not confirm closing the expected proposal",
            ));
        }
        Ok(())
    }
    fn list_reviews(&self, remote: &str, number: u64) -> Result<Vec<crate::MergeRequestReview>> {
        self.read(remote, |reader| {
            let mut result = vec![];
            let mut seen = BTreeSet::new();
            for page in 1..=super::MAX_PAGES {
                let reviews: Vec<ReviewResponse> = reader.get(&format!(
                    "/repos/{remote}/pulls/{number}/reviews?per_page=100&page={page}"
                ))?;
                let count = reviews.len();
                for review in reviews {
                    if review.id == 0 || !seen.insert(review.id) {
                        return Err(invalid("inconsistent GitHub review listing"));
                    }
                    if review.state != "PENDING" {
                        result.push(review.into_review()?);
                    }
                }
                if count < 100 {
                    return Ok(result);
                }
            }
            Err(invalid("GitHub reviews exceed 1000 entries"))
        })
    }
    fn create_comment_review(
        &self,
        remote: &str,
        number: u64,
        head: &CommitSha,
        body: &str,
    ) -> Result<crate::MergeRequestReview> {
        self.require_publication(remote)?;
        let current = self.get_merge_request(remote, number)?;
        if current.head_commit != *head || current.state != MergeRequestState::Open || current.draft
        {
            return Err(invalid("source changed before review"));
        }
        let response: ReviewResponse = self.mutate(
            remote,
            "POST",
            &format!("/repos/{remote}/pulls/{number}/reviews"),
            &json!({"event":"COMMENT","commit_id":head,"body":body}),
            200,
        )?;
        if response.author.login != self.identity.bot_login
            || response.state != "COMMENTED"
            || response.commit_id != head.as_str()
            || response.body != body
        {
            return Err(invalid(
                "GitHub review receipt differs from requested comment",
            ));
        }
        response.into_review()
    }
    fn create_approval_review(
        &self,
        remote: &str,
        number: u64,
        head: &CommitSha,
        body: &str,
    ) -> Result<crate::MergeRequestReview> {
        self.require_publication(remote)?;
        if !self.approval_repositories.contains(&remote.to_lowercase()) {
            return Err(read_only());
        }
        let current = self.get_merge_request(remote, number)?;
        if current.head_commit != *head
            || current.state != MergeRequestState::Open
            || current.draft
            || !source_checks_permit(
                current.checks,
                self.without_checks
                    .approvals
                    .permits(remote, &current.author_login),
            )
            || current.author_login == self.identity.bot_login
        {
            return Err(invalid("source changed or is not eligible for approval"));
        }
        let response: ReviewResponse = self.mutate(
            remote,
            "POST",
            &format!("/repos/{remote}/pulls/{number}/reviews"),
            &json!({"event":"APPROVE","commit_id":head,"body":body}),
            200,
        )?;
        if response.author.login != self.identity.bot_login
            || response.state != "APPROVED"
            || response.commit_id != head.as_str()
            || response.body != body
        {
            return Err(invalid(
                "GitHub approval receipt differs from requested review",
            ));
        }
        response.into_review()
    }
    fn merge(
        &self,
        remote: &str,
        number: u64,
        expected_head: &CommitSha,
        method: MergeMethod,
    ) -> Result<CommitSha> {
        self.require_publication(remote)?;
        let adaptations = self
            .adaptation_merge_repositories
            .contains(&remote.to_lowercase());
        if !self.merge_authors.covers(remote) && !adaptations {
            return Err(read_only());
        }
        let current = self.get_merge_request(remote, number)?;
        // Another author's pull request is a source merge; this bot's own is an adaptation.
        // Only a source merge whose handler opted in may proceed without checks.
        let permitted = if current
            .author_login
            .eq_ignore_ascii_case(&self.identity.bot_login)
        {
            adaptations
                && current.source_branch.starts_with("artidezi/")
                && current.checks == ChecksState::Success
        } else {
            self.merge_authors.permits(remote, &current.author_login)
                && source_checks_permit(
                    current.checks,
                    self.without_checks
                        .merges
                        .permits(remote, &current.author_login),
                )
        };
        if current.head_commit != *expected_head
            || current.state != MergeRequestState::Open
            || current.draft
            || !permitted
        {
            return Err(invalid("source changed or is not eligible for merging"));
        }
        #[derive(Deserialize)]
        struct Merged {
            merged: bool,
            sha: String,
        }
        let response: Merged = self.mutate(remote, "PUT", &format!("/repos/{remote}/pulls/{number}/merge"),
            &json!({"sha":expected_head,"merge_method":match method { MergeMethod::Merge => "merge", MergeMethod::Rebase => "rebase" }}), 200)?;
        let sha = CommitSha(response.sha);
        if !response.merged || !sha.is_well_formed() {
            return Err(invalid("GitHub did not confirm the merge"));
        }
        Ok(sha)
    }
    fn create_tag(&self, _: &str, _: &str, _: &CommitSha) -> Result<()> {
        Err(read_only())
    }
}

#[derive(Deserialize)]
struct ReviewResponse {
    id: u64,
    #[serde(rename = "user")]
    author: super::Author,
    commit_id: String,
    #[serde(default)]
    body: String,
    state: String,
}
impl ReviewResponse {
    fn into_review(self) -> Result<crate::MergeRequestReview> {
        let head_commit = CommitSha(self.commit_id);
        if self.id == 0 || !head_commit.is_well_formed() {
            return Err(invalid("invalid GitHub review receipt"));
        }
        Ok(crate::MergeRequestReview {
            state: match self.state.as_str() {
                "COMMENTED" => crate::ReviewState::Commented,
                "APPROVED" => crate::ReviewState::Approved,
                "CHANGES_REQUESTED" => crate::ReviewState::ChangesRequested,
                "DISMISSED" => crate::ReviewState::Dismissed,
                _ => crate::ReviewState::Other,
            },
            id: self.id,
            author_login: self.author.login,
            head_commit,
            body: self.body,
        })
    }
}

fn invalid(message: &str) -> ForgeError {
    ForgeError::Other(message.into())
}
fn read_only() -> ForgeError {
    invalid("GitHub operation is disabled by the adapter or repository publication scope")
}

#[cfg(test)]
mod tests;
