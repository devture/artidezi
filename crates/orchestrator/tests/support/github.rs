// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Full local GitHub intake through the actual App HTTP adapter, frozen Git bundles and worker.
//! Git transport is mapped to disposable bare repositories; no public credentials or writes.

use super::*;
use artidezi_config::model::{
    ForgeProvider, GithubApp, PullRequestAction, PullRequestDiscovery, PullRequestHandler,
};
use artidezi_contract::common::CommitSha;
use artidezi_forge::github::{
    GithubForge,
    fixture::{Fixture, Reply, State},
};
use artidezi_forge::{MergeMethod, MergeRequestInfo, NewMergeRequest};
use serde_json::json;

#[path = "github_ci.rs"]
mod ci;
#[path = "github_supersession.rs"]
mod supersession;

struct LocalGit {
    github: GithubForge,
    git: FakeForge,
    state: std::sync::Arc<std::sync::Mutex<State>>,
}
impl LocalGit {
    fn sync_branch(&self, remote: &str, branch: &str) {
        if let Ok(head) = self.git.branch_head(remote, branch) {
            if branch == "main" {
                self.state.lock().unwrap().base = head.0.clone();
            }
            self.state
                .lock()
                .unwrap()
                .branches
                .insert(branch.into(), head.0);
        }
    }
}
impl ForgeAdapter for LocalGit {
    fn protected_paths(&self) -> &'static [&'static str] {
        self.github.protected_paths()
    }
    fn ci_job_logs(
        &self,
        remote: &str,
        head: &CommitSha,
        names: &[String],
    ) -> Vec<artidezi_forge::CiJobLog> {
        self.github.ci_job_logs(remote, head, names)
    }
    fn writes_allowed(&self) -> bool {
        self.github.writes_allowed()
    }
    fn writes_allowed_for(&self, r: &str) -> bool {
        self.github.writes_allowed_for(r)
    }
    fn git_push_credentials(
        &self,
        r: &str,
    ) -> artidezi_forge::Result<Option<artidezi_git::cache::HttpsCredentials>> {
        self.github.git_push_credentials(r)?;
        Ok(None)
    }
    fn git_push_workflow_credentials(
        &self,
        r: &str,
    ) -> artidezi_forge::Result<Option<artidezi_git::cache::HttpsCredentials>> {
        self.github.git_push_workflow_credentials(r)?;
        Ok(None)
    }
    fn proposals_for_branch(
        &self,
        r: &str,
        b: &str,
    ) -> artidezi_forge::Result<Vec<MergeRequestInfo>> {
        self.sync_branch(r, b);
        self.github.proposals_for_branch(r, b)
    }
    fn list_reviews(
        &self,
        r: &str,
        n: u64,
    ) -> artidezi_forge::Result<Vec<artidezi_forge::MergeRequestReview>> {
        self.github.list_reviews(r, n)
    }
    fn create_comment_review(
        &self,
        r: &str,
        n: u64,
        h: &CommitSha,
        b: &str,
    ) -> artidezi_forge::Result<artidezi_forge::MergeRequestReview> {
        self.github.create_comment_review(r, n, h, b)
    }
    fn create_approval_review(
        &self,
        r: &str,
        n: u64,
        h: &CommitSha,
        b: &str,
    ) -> artidezi_forge::Result<artidezi_forge::MergeRequestReview> {
        self.github.create_approval_review(r, n, h, b)
    }
    fn retry_after(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.github.retry_after()
    }
    fn clone_url(&self, remote: &str) -> String {
        self.git.clone_url(remote)
    }
    fn git_credentials(
        &self,
        remote: &str,
    ) -> artidezi_forge::Result<Option<artidezi_git::cache::HttpsCredentials>> {
        self.github.git_credentials(remote)?; // Verify scoped authentication; local Git needs none.
        Ok(None)
    }
    fn branch_head(&self, r: &str, b: &str) -> artidezi_forge::Result<CommitSha> {
        self.sync_branch(r, b);
        self.github.branch_head(r, b)
    }
    fn merge_request_ref(&self, r: &str, n: u64) -> artidezi_forge::Result<String> {
        self.github.merge_request_ref(r, n)
    }
    fn get_merge_request(&self, r: &str, n: u64) -> artidezi_forge::Result<MergeRequestInfo> {
        self.github.get_merge_request(r, n)
    }
    fn list_open_merge_requests(
        &self,
        r: &str,
        a: Option<&str>,
    ) -> artidezi_forge::Result<Vec<MergeRequestInfo>> {
        self.github.list_open_merge_requests(r, a)
    }
    fn list_open_merge_request_summaries(
        &self,
        r: &str,
        a: Option<&str>,
    ) -> artidezi_forge::Result<Vec<artidezi_forge::MergeRequestListing>> {
        self.github.list_open_merge_request_summaries(r, a)
    }
    fn create_merge_request(
        &self,
        r: &str,
        request: &NewMergeRequest,
    ) -> artidezi_forge::Result<MergeRequestInfo> {
        self.sync_branch(r, &request.source_branch);
        self.github.create_merge_request(r, request)
    }
    fn update_merge_request(
        &self,
        r: &str,
        n: u64,
        title: &str,
        body: &str,
    ) -> artidezi_forge::Result<()> {
        self.github.update_merge_request(r, n, title, body)
    }
    fn close_merge_request(
        &self,
        r: &str,
        expected: &artidezi_forge::ExpectedProposal<'_>,
        comment: &str,
    ) -> artidezi_forge::Result<()> {
        self.github.close_merge_request(r, expected, comment)
    }
    fn merge(
        &self,
        r: &str,
        n: u64,
        h: &CommitSha,
        m: MergeMethod,
    ) -> artidezi_forge::Result<CommitSha> {
        self.github.merge(r, n, h, m)
    }
    fn create_tag(&self, _: &str, _: &str, _: &CommitSha) -> artidezi_forge::Result<()> {
        panic!("unexpected tag")
    }
    fn delete_branch(&self, _: &str, _: &str) -> artidezi_forge::Result<()> {
        panic!("unexpected delete")
    }
}

fn reopen(w: &mut World, fixture: &Fixture) {
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert("fake".into(), Box::new(w.forge.clone()));
    forges.insert(
        "github".into(),
        Box::new(LocalGit {
            github: if fixture.state.lock().unwrap().publication {
                fixture
                    .forge()
                    .with_publication_repositories(["org/app".into()].into())
                    .unwrap()
                    .with_source_actions(
                        if w.orchestrator.config.projects["roles"]
                            .pull_requests
                            .as_ref()
                            .unwrap()
                            .handlers[0]
                            .safe_verdict_action
                            != artidezi_config::model::SafeVerdictAction::Comment
                        {
                            ["org/app".into()].into()
                        } else {
                            Default::default()
                        },
                        if w.orchestrator.config.projects["roles"]
                            .pull_requests
                            .as_ref()
                            .unwrap()
                            .handlers[0]
                            .safe_verdict_action
                            == artidezi_config::model::SafeVerdictAction::Merge
                        {
                            ["org/app".into()].into()
                        } else {
                            Default::default()
                        },
                    )
                    .unwrap()
            } else {
                fixture.forge()
            },
            git: w.forge.clone(),
            state: fixture.state.clone(),
        }),
    );
    w.orchestrator =
        Orchestrator::open(w.orchestrator.config.clone(), &w.root.join("data"), forges).unwrap();
}
fn world() -> (World, Fixture) {
    let mut w = World::new();
    let repo = w.forge.ensure_repository("org/app").unwrap();
    let base = seed(&repo, None, "defaults.yml", "version: 1.0.0\n", "Initial");
    let head = seed(
        &repo,
        Some(base),
        "defaults.yml",
        "version: 1.1.0\n",
        "Update library",
    );
    repo.reference("refs/heads/main", base, true, "target")
        .unwrap();
    repo.reference("refs/pull/33/head", head, true, "fork input")
        .unwrap();
    let fixture = Fixture::new(&base.to_string(), &head.to_string());
    let mut forge = w.orchestrator.config.forges["fake"].clone();
    forge.provider = ForgeProvider::Github;
    forge.api_base_url = "https://api.github.com".into();
    forge.bot_login = "artidezi-fixture[bot]".into();
    forge.credential_ref = "app-key".into();
    forge.github_app = Some(GithubApp {
        client_id: "Iv.fixture".into(),
        installation_id: 42,
        account: "org".into(),
    });
    w.orchestrator.config.forges.insert("github".into(), forge);
    let p = w.orchestrator.config.projects.get_mut("roles").unwrap();
    p.effects.publish_adaptation_merge_requests = false;
    let r = p.repositories.get_mut("role").unwrap();
    r.forge = "github".into();
    r.remote = "org/app".into();
    r.tagging = None;
    p.pull_requests = Some(PullRequestDiscovery {
        poll_interval_seconds: 600,
        settle_seconds: 0,
        max_runs_per_pull_request: 3,
        max_active_tasks: 1,
        repositories: vec!["role".into()],
        ci_log_context: Default::default(),
        handlers: vec![PullRequestHandler {
            agent_profile: None,
            safe_verdict_action: Default::default(),
            mode: Default::default(),
            id: "renovate".into(),
            authors: Some(vec!["renovate[bot]".into()]),
            title_regex: None,
            catch_all: false,
            action: PullRequestAction::Assess,
            wait_for_checks_to_finish: true,
            require_successful_checks: true,
            ci_context: artidezi_config::model::PullRequestCiContext {
                results: true,
                logs: false,
            },
            instructions: "Inspect the update and consumer context. Dry run only.".into(),
        }],
    });
    assert!(artidezi_config::validate(&w.orchestrator.config).is_empty());
    reopen(&mut w, &fixture);
    (w, fixture)
}
fn scan(w: &mut World) -> Vec<String> {
    let report = w
        .orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report.queued
}

#[test]
fn selected_failed_log_is_frozen_as_private_evidence_and_rendered_only_for_the_worker() {
    let (mut w, fixture) = world();
    {
        let mut state = fixture.state.lock().unwrap();
        state.actions_permission = true;
        state.conclusion = "failure".into();
    }
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.ci_log_context.permitted = true;
    discovery.handlers[0].require_successful_checks = false;
    discovery.handlers[0].ci_context.logs = true;
    assert!(artidezi_config::validate(&w.orchestrator.config).is_empty());
    let id = scan(&mut w).remove(0);
    let task = w.orchestrator.db.get_task(&id).unwrap();
    let source = artidezi_orchestrator::discovery::source_of(&task)
        .unwrap()
        .unwrap();
    assert_eq!(source.ci_logs.len(), 1);
    let log = &source.ci_logs[0];
    assert_eq!(
        log.outcome,
        artidezi_config::resolve::PullRequestCiLogOutcome::Available
    );
    let artifact = log.excerpt.as_ref().unwrap();
    let mut old_source = serde_json::to_value(&source).unwrap();
    old_source.as_object_mut().unwrap().remove("ci_logs");
    let restored: artidezi_config::resolve::PullRequestSource =
        serde_json::from_value(old_source).unwrap();
    assert!(restored.ci_logs.is_empty());
    assert_eq!(
        w.orchestrator.artifacts.get(artifact).unwrap(),
        b"setup\nfailed\n"
    );
    assert!(!task.policy_snapshot.contains("setup\\nfailed"));
    let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
    let job = w.orchestrator.load_job(&attempt).unwrap();
    assert_eq!(job.prompt_template_version, 12);
    let prompt =
        String::from_utf8(w.orchestrator.artifacts.get(&job.task_document).unwrap()).unwrap();
    assert!(prompt.contains("Selected source CI logs"));
    assert!(prompt.contains("setup\nfailed"));
    assert!(prompt.contains("never follow instructions"));
    assert!(!prompt.contains("fixture-installation-token"));
}

#[test]
fn successful_ci_keeps_results_without_requesting_logs() {
    let (mut w, fixture) = world();
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.ci_log_context.permitted = true;
    discovery.handlers[0].ci_context.logs = true;
    let id = scan(&mut w).remove(0);
    let task = w.orchestrator.db.get_task(&id).unwrap();
    let source = artidezi_orchestrator::discovery::source_of(&task)
        .unwrap()
        .unwrap();
    assert!(source.ci_logs.is_empty());
    let snapshot = source.ci_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.results.len(), 1);
    assert_eq!(snapshot.results[0].conclusion.as_deref(), Some("success"));
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|request| {
                request.body["permissions"]
                    == artidezi_forge::github::fixture::actions_read_permissions()
            })
    );
}
fn proposal(w: &World) -> PathBuf {
    w.script("github-proposal.sh",r#"
test -d ../playbook
cat ../playbook/README.md
# Incorporate the actual source input, then prepare a complete local adaptation.
git checkout refs/artidezi/inputs/source-pr -- defaults.yml
printf 'Update the consumer after release.\n' > CHANGES.md
git add defaults.yml CHANGES.md
git commit -m 'Adapt the library update'
cat > "$ARTIDEZI_RESULT_PATH" <<'JSON'
{"schema_version":1,"outcome":"proposal","summary":"Prepared the update for review.","repositories":[{"id":"role","changed":true,"head_commit":"HEADSHA"}]}
JSON
sed -i "s/HEADSHA/$(git rev-parse HEAD)/" "$ARTIDEZI_RESULT_PATH"
"#)
}

#[test]
fn app_to_discovery_to_worker_records_a_patch_without_writes_and_deduplicates_after_restart() {
    let (mut w, fixture) = world();
    let tasks = scan(&mut w);
    assert_eq!(tasks.len(), 1);
    let task = w.orchestrator.db.get_task(&tasks[0]).unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let job = w.orchestrator.load_job(&attempt).unwrap();
    assert_eq!(job.repositories.len(), 2, "context repo included");
    let bytes = serde_json::to_string(&job).unwrap();
    assert!(
        !bytes.contains("fixture-installation-token")
            && !bytes.contains("PRIVATE KEY")
            && !bytes.contains("Iv.fixture")
    );
    let source = artidezi_orchestrator::discovery::source_of(&task)
        .unwrap()
        .unwrap();
    assert_eq!(
        source.input.commit.as_str(),
        fixture.state.lock().unwrap().head
    );
    let ci = source.ci_snapshot.as_ref().unwrap();
    assert_eq!(
        ci.overall,
        artidezi_config::resolve::PullRequestCiState::Success
    );
    assert_eq!(ci.results[0].name, "tests");
    assert_eq!(ci.results[0].conclusion.as_deref(), Some("success"));
    let prompt =
        String::from_utf8(w.orchestrator.artifacts.get(&job.task_document).unwrap()).unwrap();
    assert!(prompt.contains("Source CI snapshot (untrusted forge data)"));
    assert!(prompt.contains("\"name\": \"tests\""));
    let script = proposal(&w);
    let judged = w.work(&script);
    assert_eq!(
        judged,
        AttemptState::Valid,
        "{:?}",
        w.orchestrator
            .db
            .list_tasks(None)
            .unwrap()
            .iter()
            .map(|t| w
                .orchestrator
                .db
                .latest_attempt(&t.id)
                .unwrap()
                .map(|a| a.validation_problems))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::Done
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );
    let repo = w.forge.ensure_repository("org/app").unwrap();
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap().to_string(),
        fixture.state.lock().unwrap().base
    );
    assert_eq!(
        repo.references_glob("refs/heads/artidezi/*")
            .unwrap()
            .count(),
        0
    );
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == "GET" || r.path == "/app/installations/42/access_tokens")
    );
    reopen(&mut w, &fixture);
    assert!(scan(&mut w).is_empty());
    fixture.state.lock().unwrap().requests.clear();
    assert!(scan(&mut w).is_empty());
    let state = fixture.state.lock().unwrap();
    let reads = state
        .requests
        .iter()
        .filter(|request| request.method == "GET" && request.path.starts_with("/repos/org/app"))
        .collect::<Vec<_>>();
    assert_eq!(
        reads.len(),
        1,
        "an unchanged completed source needs only the open listing"
    );
    assert!(reads[0].path.contains("/pulls?"));
    drop(state);
    w.orchestrator
        .pause_pull_request(&source.tracking_id, "human")
        .unwrap();
    assert!(scan(&mut w).is_empty());
    w.orchestrator
        .resume_pull_request(&source.tracking_id, "human")
        .unwrap();
    assert_eq!(scan(&mut w).len(), 1);
}

#[test]
fn pending_checks_wait_and_moved_source_results_are_not_current() {
    let (mut w, fixture) = world();
    fixture.state.lock().unwrap().conclusion = "running".into();
    assert!(scan(&mut w).is_empty());
    fixture.state.lock().unwrap().conclusion = "success".into();
    let task = scan(&mut w).remove(0);
    fixture.state.lock().unwrap().head = "c".repeat(40);
    let script = proposal(&w);
    let judged = w.work(&script);
    assert_eq!(
        judged,
        AttemptState::Valid,
        "{:?}",
        w.orchestrator
            .db
            .list_tasks(None)
            .unwrap()
            .iter()
            .map(|t| w
                .orchestrator
                .db
                .latest_attempt(&t.id)
                .unwrap()
                .map(|a| a.validation_problems))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task).unwrap().state,
        TaskState::Closed
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn backoff_survives_restart_and_scan_now_does_not_bypass_it() {
    let (mut w, fixture) = world();
    let mut reply = Reply::new(429, json!({"message":"temporary limit"}));
    reply.headers.insert("retry-after".into(), "600".into());
    fixture
        .state
        .lock()
        .unwrap()
        .overrides
        .insert("/repos/org/app".into(), reply);
    let report = w
        .orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.queued.is_empty());
    assert!(
        w.orchestrator
            .db
            .forge_retry_after("github")
            .unwrap()
            .is_some()
    );
    let count = fixture.state.lock().unwrap().requests.len();
    reopen(&mut w, &fixture);
    assert!(
        !w.orchestrator
            .scan_pull_requests(Some("roles"), true)
            .unwrap()
            .errors
            .is_empty()
    );
    assert_eq!(fixture.state.lock().unwrap().requests.len(), count);
    assert!(
        w.orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn older_queued_publication_cannot_write_after_a_forge_becomes_read_only() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing", "human", &TaskSelection::default())
        .unwrap();
    w.orchestrator
        .config
        .forges
        .get_mut("fake")
        .unwrap()
        .provider = ForgeProvider::Github;
    let script = w.script("blocked.sh", &proposal_script(false));
    let judged = w.work(&script);
    assert_eq!(
        judged,
        AttemptState::Valid,
        "{:?}",
        w.orchestrator
            .db
            .list_tasks(None)
            .unwrap()
            .iter()
            .map(|t| w
                .orchestrator
                .db
                .latest_attempt(&t.id)
                .unwrap()
                .map(|a| a.validation_problems))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    assert!(
        w.forge
            .list_open_merge_requests("org/role", None)
            .unwrap()
            .is_empty()
    );
    assert!(
        w.orchestrator
            .request_revision(&task.id, "Try again", "human")
            .is_err()
    );
    assert_eq!(
        w.forge
            .ensure_repository("org/role")
            .unwrap()
            .references_glob("refs/heads/artidezi/*")
            .unwrap()
            .count(),
        0
    );
}

fn publishing_world() -> (World, Fixture) {
    let (mut w, fixture) = world();
    fixture.state.lock().unwrap().publication = true;
    let p = w.orchestrator.config.projects.get_mut("roles").unwrap();
    p.effects.publish_adaptation_merge_requests = true;
    p.pull_requests.as_mut().unwrap().handlers[0].mode =
        artidezi_config::model::PullRequestMode::Publish;
    reopen(&mut w, &fixture);
    (w, fixture)
}
fn unchanged(w: &World) -> PathBuf {
    w.script("unchanged.sh", &result_writer(r#"{"schema_version":1,"outcome":"approve_untouched","summary":"No adaptations necessary.","repositories":[{"id":"role","changed":false}]}"#))
}
fn task_state(w: &World, id: &str) -> TaskState {
    w.orchestrator.db.get_task(id).unwrap().state
}
fn no_publication(f: &Fixture) {
    let state = f.state.lock().unwrap();
    assert!(state.proposals.is_empty());
    assert!(state.reviews.is_empty());
    assert!(
        state
            .requests
            .iter()
            .all(|r| r.method == "GET" || r.path.starts_with("/app/"))
    );
}

#[test]
fn live_unchanged_result_posts_only_a_comment_review_and_deduplicates() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&unchanged(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::Done);
    let state = f.state.lock().unwrap();
    assert_eq!(state.reviews.len(), 1);
    assert_eq!(state.reviews[0]["state"], "COMMENTED");
    assert!(state.proposals.is_empty());
    drop(state);
    reopen(&mut w, &f);
    assert!(scan(&mut w).is_empty());
    assert!(w.orchestrator.retry_publication(&id, "human").is_err());
}

#[test]
fn published_adaptation_can_be_revised_in_place() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    let first = w
        .orchestrator
        .db
        .list_merge_requests(&id)
        .unwrap()
        .remove(0);

    w.orchestrator
        .revise_proposal(&id, "Adjust the change", "human")
        .unwrap();
    let script = std::fs::read_to_string(proposal(&w))
        .unwrap()
        .replace("Update the consumer", "Update the revised consumer");
    let revised = w.script("github-revision.sh", &script);
    assert_eq!(w.work(&revised), AttemptState::Valid);

    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    let revised = w
        .orchestrator
        .db
        .list_merge_requests(&id)
        .unwrap()
        .remove(0);
    assert_eq!(revised.number, first.number);
    assert_ne!(revised.head_commit, first.head_commit);
    assert_eq!(f.state.lock().unwrap().proposals.len(), 1);
}

#[test]
fn live_adaptation_survives_lost_pr_and_review_responses_without_duplicate_effects() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().lose_create_response = true;
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(f.state.lock().unwrap().proposals.len(), 1);
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .is_empty()
    );
    // Lost receipt must not let feedback create a duplicate PR under another task.
    assert!(
        w.orchestrator
            .request_revision(&id, "Try again", "human")
            .is_err()
    );
    let tracked =
        artidezi_orchestrator::discovery::source_of(&w.orchestrator.db.get_task(&id).unwrap())
            .unwrap()
            .unwrap();
    assert!(
        w.orchestrator
            .resume_pull_request(&tracked.tracking_id, "human")
            .is_err()
    );
    // Model a process disappearing between upload validation and final state transition.
    w.orchestrator
        .db
        .set_task_state(&id, TaskState::Working, "fixture")
        .unwrap();
    let old_attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap().id;
    reopen(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    f.state.lock().unwrap().lose_review_response = true;
    assert!(w.orchestrator.retry_publication(&id, "human").is_err());
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
    assert_eq!(w.orchestrator.db.list_merge_requests(&id).unwrap().len(), 1);
    reopen(&mut w, &f);
    w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    assert_eq!(
        w.orchestrator.db.latest_attempt(&id).unwrap().unwrap().id,
        old_attempt
    );
    let state = f.state.lock().unwrap();
    assert_eq!(state.proposals.len(), 1);
    assert_eq!(state.reviews.len(), 1);
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/repos/org/app/pulls")
            .count(),
        1
    );
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/reviews"))
            .count(),
        1
    );
    assert!(state.open);
    assert_eq!(
        w.forge.branch_head("org/app", "main").unwrap().as_str(),
        state.base
    );
    drop(state);
    assert!(scan(&mut w).is_empty());
}

#[test]
fn source_publication_rechecks_source_context_and_configuration() {
    for change in ["source", "context", "mode", "bot", "scope", "source_closed"] {
        let (mut w, f) = publishing_world();
        let id = scan(&mut w).remove(0);
        match change {
            "source" => f.state.lock().unwrap().head = "c".repeat(40),
            "context" => {
                let r = w.forge.ensure_repository("org/playbook").unwrap();
                let base = r.refname_to_id("refs/heads/main").unwrap();
                seed(&r, Some(base), "README.md", "changed", "Context moved");
            }
            "mode" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .pull_requests
                    .as_mut()
                    .unwrap()
                    .handlers[0]
                    .mode = Default::default()
            }
            "bot" => {
                w.orchestrator
                    .config
                    .forges
                    .get_mut("github")
                    .unwrap()
                    .bot_login = "replacement[bot]".into()
            }
            "scope" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .repositories
                    .get_mut("role")
                    .unwrap()
                    .agent_access = artidezi_contract::common::RepositoryAccess::ReadOnly
            }
            "source_closed" => f.state.lock().unwrap().open = false,
            _ => unreachable!(),
        }
        assert_eq!(w.work(&proposal(&w)), AttemptState::Valid, "{change}");
        assert_eq!(
            task_state(&w, &id),
            if change == "source" {
                TaskState::Closed
            } else if change == "context" {
                TaskState::AwaitingReview
            } else {
                TaskState::NeedsHuman
            },
            "{change}"
        );
        if change == "context" {
            assert!(!f.state.lock().unwrap().proposals.is_empty());
        } else {
            no_publication(&f);
        }
    }
}

#[test]
fn closed_source_retires_an_unfinished_assessment() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);

    f.state.lock().unwrap().open = false;
    assert!(scan(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert!(
        w.orchestrator
            .db
            .list_events(&id)
            .unwrap()
            .iter()
            .any(|event| event.kind == "assessment.closed_source")
    );

    f.state.lock().unwrap().requests.clear();
    assert!(scan(&mut w).is_empty());
    let state = f.state.lock().unwrap();
    let requests = &state.requests;
    assert!(
        requests
            .iter()
            .any(|request| request.path.starts_with("/repos/org/app/pulls?"))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.path == "/repos/org/app/pulls/33")
    );
    drop(state);

    f.state.lock().unwrap().open = true;
    scan(&mut w);
    assert!(
        !w.orchestrator
            .db
            .list_pull_request_sources(Some("roles"))
            .unwrap()[0]
            .closed
    );
}

#[test]
fn reopened_new_head_renews_a_resolved_lifecycle_and_preserves_history() {
    for merged in [false, true] {
        let (mut w, f) = publishing_world();
        let id = scan(&mut w).remove(0);
        assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
        let old_task = w.orchestrator.db.get_task(&id).unwrap();
        let old_source = artidezi_orchestrator::discovery::source_of(&old_task)
            .unwrap()
            .unwrap();
        let old_attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        let old_proposals = w.orchestrator.db.list_merge_requests(&id).unwrap();
        {
            let mut state = f.state.lock().unwrap();
            for proposal in state.proposals.values_mut() {
                proposal["state"] = json!("closed");
                proposal["merged"] = json!(merged);
            }
            state.open = false;
        }
        assert!(scan(&mut w).is_empty());
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        let mut tracked = w
            .orchestrator
            .db
            .get_pull_request_source(&old_source.tracking_id)
            .unwrap();
        assert!(tracked.closed);
        // Exercise both the reported one-run pause and an exhausted budget.
        tracked.runs = if merged { 1 } else { 3 };
        tracked.paused = true;
        w.orchestrator
            .db
            .save_pull_request_source(&tracked)
            .unwrap();
        let newer = move_source_head(&w, &f);
        f.state.lock().unwrap().open = true;
        w.orchestrator
            .config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .settle_seconds = 10;
        reopen(&mut w, &f);
        let now = chrono::Utc::now();
        let report = w
            .orchestrator
            .scan_pull_requests_at(Some("roles"), true, now)
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.queued.is_empty());
        let fresh = w
            .orchestrator
            .db
            .get_pull_request_source(&tracked.id)
            .unwrap();
        assert!(!fresh.closed);
        assert!(!fresh.paused);
        assert_eq!(fresh.runs, 0);
        assert_eq!(fresh.last_task_id, None);
        assert_eq!(fresh.observed_head, newer.0);
        assert_eq!(fresh.head_observed_at, now);
        assert_eq!(fresh.follow_up.unwrap().at, now + Duration::seconds(10));

        let report = w
            .orchestrator
            .scan_pull_requests_at(Some("roles"), true, now + Duration::seconds(10))
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.queued.len(), 1);
        let next = &report.queued[0];
        assert_ne!(next, &id);
        let fresh = w
            .orchestrator
            .db
            .get_pull_request_source(&tracked.id)
            .unwrap();
        assert_eq!(fresh.runs, 1);
        assert_eq!(fresh.last_task_id.as_ref(), Some(next));
        let task = w.orchestrator.db.get_task(next).unwrap();
        assert_eq!(
            artidezi_orchestrator::discovery::source_of(&task)
                .unwrap()
                .unwrap()
                .input
                .commit,
            newer
        );
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        assert_eq!(
            w.orchestrator.db.get_task(&id).unwrap().policy_snapshot,
            old_task.policy_snapshot
        );
        assert_eq!(
            w.orchestrator.db.latest_attempt(&id).unwrap().unwrap().id,
            old_attempt.id
        );
        assert_eq!(
            w.orchestrator.db.list_merge_requests(&id).unwrap(),
            old_proposals
        );
        assert_eq!(w.work(&unchanged(&w)), AttemptState::Valid);
        reopen(&mut w, &f);
        assert!(
            scan(&mut w).is_empty(),
            "completed reopened head stays deduplicated after restart"
        );
    }
}

fn move_source_head(w: &World, f: &Fixture) -> artidezi_contract::common::CommitSha {
    let repo = w.forge.ensure_repository("org/app").unwrap();
    let base = repo.refname_to_id("refs/heads/main").unwrap();
    let next = seed(
        &repo,
        Some(base),
        "defaults.yml",
        "version: 2.0.0\n",
        "New source revision",
    );
    repo.reference("refs/heads/main", base, true, "restore target")
        .unwrap();
    repo.reference("refs/pull/33/head", next, true, "new source head")
        .unwrap();
    f.state.lock().unwrap().head = next.to_string();
    artidezi_contract::common::CommitSha(next.to_string())
}

#[test]
fn same_head_reopening_and_open_source_changes_keep_holds_and_budget() {
    for close in [false, true] {
        let (mut w, f) = world();
        let id = scan(&mut w).remove(0);
        assert_eq!(w.work(&unchanged(&w)), AttemptState::Valid);
        if close {
            f.state.lock().unwrap().open = false;
            assert!(scan(&mut w).is_empty());
        } else {
            move_source_head(&w, &f);
        }
        let mut tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        tracked.paused = true;
        tracked.runs = 3;
        w.orchestrator
            .db
            .save_pull_request_source(&tracked)
            .unwrap();
        f.state.lock().unwrap().open = true;
        assert!(scan(&mut w).is_empty());
        let current = w
            .orchestrator
            .db
            .get_pull_request_source(&tracked.id)
            .unwrap();
        assert!(!current.closed);
        assert!(current.paused);
        assert_eq!(current.runs, 3);
        assert_eq!(current.last_task_id.as_deref(), Some(id.as_str()));
    }
}

#[test]
fn reopened_new_head_keeps_a_nonterminal_delivery_task_linked() {
    let (mut w, f) = safe_world(true);
    f.state.lock().unwrap().pending_on_approval = true;
    let id = scan(&mut w).remove(0);
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    f.state.lock().unwrap().open = false;
    assert!(scan(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    let tracked = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert!(tracked.closed);
    move_source_head(&w, &f);
    f.state.lock().unwrap().open = true;
    assert!(scan(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    w.orchestrator
        .tick_at(chrono::Utc::now() + chrono::Duration::seconds(601))
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let held = w
        .orchestrator
        .db
        .get_pull_request_source(&tracked.id)
        .unwrap();
    assert!(held.paused);
    assert_eq!(held.runs, tracked.runs);
    assert_eq!(held.last_task_id, tracked.last_task_id);
}

#[test]
fn reopened_new_head_cannot_bypass_open_adaptations_including_lost_receipts() {
    for lost_receipt in [false, true] {
        let (mut w, f) = publishing_world();
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().lose_create_response = lost_receipt;
        assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
        assert_eq!(
            w.orchestrator
                .db
                .list_merge_requests(&id)
                .unwrap()
                .is_empty(),
            lost_receipt
        );
        f.state.lock().unwrap().open = false;
        assert!(scan(&mut w).is_empty());
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        let tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        move_source_head(&w, &f);
        f.state.lock().unwrap().open = true;
        let report = w
            .orchestrator
            .scan_pull_requests(Some("roles"), true)
            .unwrap();
        assert!(report.queued.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("existing adaptation proposals"));
        let held = w
            .orchestrator
            .db
            .get_pull_request_source(&tracked.id)
            .unwrap();
        assert!(held.paused);
        assert_eq!(held.runs, tracked.runs);
        assert_eq!(held.last_task_id, tracked.last_task_id);
        assert!(scan(&mut w).is_empty());
    }
}

#[test]
fn old_dry_run_is_never_promoted_and_workflow_changes_are_withheld() {
    let (mut w, f) = world();
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().publication = true;
    let p = w.orchestrator.config.projects.get_mut("roles").unwrap();
    p.effects.publish_adaptation_merge_requests = true;
    p.pull_requests.as_mut().unwrap().handlers[0].mode =
        artidezi_config::model::PullRequestMode::Publish;
    reopen(&mut w, &f);
    w.work(&proposal(&w));
    assert_eq!(task_state(&w, &id), TaskState::Done);
    no_publication(&f);
    assert!(w.orchestrator.retry_publication(&id, "human").is_err());

    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    let path = proposal(&w);
    let script=fs::read_to_string(&path).unwrap().replace("git add defaults.yml CHANGES.md", "mkdir -p .github/workflows\nprintf 'name: injected\\n' > .github/workflows/job.yml\ngit add defaults.yml CHANGES.md .github/workflows/job.yml");
    fs::write(&path, script).unwrap();
    assert_eq!(w.work(&path), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    no_publication(&f);
    assert!(w.orchestrator.retry_publication(&id, "human").is_err());
    no_publication(&f);
}

#[test]
fn exact_workflow_grant_retries_and_recovers_without_a_second_proposal() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    let path = proposal(&w);
    let script = fs::read_to_string(&path).unwrap().replace(
        "git add defaults.yml CHANGES.md",
        "mkdir -p .github/workflows\nprintf 'name: reviewed\\n' > .github/workflows/job.yml\ngit add defaults.yml CHANGES.md .github/workflows/job.yml",
    );
    fs::write(&path, script).unwrap();
    assert_eq!(w.work(&path), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
    let digest = attempt.attempt_record.as_ref().unwrap().sha256.clone();
    assert!(
        w.orchestrator
            .authorize_workflow_publication(&id, &attempt.id, &"0".repeat(64), "reviewed", "human")
            .is_err()
    );
    assert!(
        w.orchestrator
            .authorize_workflow_publication(&id, "another-attempt", &digest, "reviewed", "human")
            .is_err()
    );
    no_publication(&f);
    assert!(
        w.orchestrator
            .db
            .list_events(&id)
            .unwrap()
            .iter()
            .all(|event| event.kind != "publication.workflow_authorized")
    );
    {
        let mut state = f.state.lock().unwrap();
        state.workflow_permission = true;
        state.lose_create_response = true;
    }
    assert!(
        w.orchestrator
            .authorize_workflow_publication(
                &id,
                &attempt.id,
                &digest,
                "Reviewed the exact workflow change",
                "human"
            )
            .is_err()
    );
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(f.state.lock().unwrap().proposals.len(), 1);
    let grants: Vec<_> = w
        .orchestrator
        .db
        .list_events(&id)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "publication.workflow_authorized")
        .collect();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].actor, "human");
    assert_eq!(grants[0].attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(grants[0].payload["attempt_record_sha256"], digest);
    reopen(&mut w, &f);
    w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    assert_eq!(
        w.orchestrator.db.latest_attempt(&id).unwrap().unwrap().id,
        attempt.id
    );
    let state = f.state.lock().unwrap();
    assert_eq!(state.proposals.len(), 1);
    assert_eq!(state.reviews.len(), 1);
}

fn safe_world(merge: bool) -> (World, Fixture) {
    let (mut w, f) = publishing_world();
    let p = w.orchestrator.config.projects.get_mut("roles").unwrap();
    p.effects.merge_renovate_merge_request_on_safe_verdict = merge;
    let h = &mut p.pull_requests.as_mut().unwrap().handlers[0];
    h.safe_verdict_action = if merge {
        artidezi_config::model::SafeVerdictAction::Merge
    } else {
        artidezi_config::model::SafeVerdictAction::Approve
    };
    h.title_regex = Some("^Update library$".into());
    // Red CI may be assessed for adaptation; delivery must still require green CI.
    h.require_successful_checks = false;
    assert!(
        artidezi_config::validate(&w.orchestrator.config).is_empty(),
        "{:?}",
        artidezi_config::validate(&w.orchestrator.config)
    );
    reopen(&mut w, &f);
    (w, f)
}
fn safe_result(w: &World, risk: Option<&str>, evidence: bool) -> PathBuf {
    let mut result = json!({"schema_version":1,"outcome":"approve_untouched","summary":"No adaptations necessary.","repositories":[{"id":"role","changed":false}]});
    if let Some(risk) = risk {
        result["risk_hint"] =
            json!({"level":risk,"rationale":"Reviewed upstream changes and tested the update"});
    }
    if evidence {
        result["evidence"] =
            json!([{"kind":"local_check","source":"fixture check","note":"passed"}]);
    }
    w.script("safe-result.sh", &result_writer(&result.to_string()))
}
#[test]
fn safe_delivery_approves_or_merges_only_when_explicitly_selected() {
    for merge in [false, true] {
        let (mut w, f) = safe_world(merge);
        let id = scan(&mut w).remove(0);
        assert_eq!(
            w.work(&safe_result(&w, Some("low"), true)),
            AttemptState::Valid
        );
        assert_eq!(task_state(&w, &id), TaskState::Done);
        let s = f.state.lock().unwrap();
        assert_eq!(s.reviews.len(), 1);
        assert_eq!(s.reviews[0]["state"], "APPROVED");
        assert_eq!(s.merged, merge);
        assert_eq!(
            s.requests.iter().filter(|r| r.method == "PUT").count(),
            usize::from(merge)
        );
        assert!(s.proposals.is_empty());
        drop(s);
        reopen(&mut w, &f);
        assert!(scan(&mut w).is_empty());
    }
}
#[test]
fn safe_delivery_blocks_checks_objections_and_changed_authority() {
    for reason in [
        "checks_pending",
        "checks_failed",
        "objection",
        "action_changed",
        "merge_revoked",
        "head_changed",
        "title_changed",
        "target_changed",
    ] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        let mut risk = Some("low");
        let mut evidence = true;
        match reason {
            "risk" => risk = Some("medium"),
            "missing_risk" => risk = None,
            "evidence" => evidence = false,
            "checks_pending" => f.state.lock().unwrap().conclusion = "running".into(),
            "checks_failed" => f.state.lock().unwrap().conclusion = "failure".into(),
            "objection" => {
                let mut s = f.state.lock().unwrap();
                let head = s.head.clone();
                s.reviews.push(json!({"id":50,"user":{"login":"maintainer","type":"User"},"commit_id":head,"body":"Needs a fix","state":"CHANGES_REQUESTED"}));
            }
            "action_changed" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .pull_requests
                    .as_mut()
                    .unwrap()
                    .handlers[0]
                    .safe_verdict_action = Default::default()
            }
            "merge_revoked" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .effects
                    .merge_renovate_merge_request_on_safe_verdict = false
            }
            "head_changed" => f.state.lock().unwrap().head = "d".repeat(40),
            "title_changed" => f.state.lock().unwrap().title = "Update another library".into(),
            "target_changed" => {
                let repo = w.forge.ensure_repository("org/app").unwrap();
                let base = repo.refname_to_id("refs/heads/main").unwrap();
                seed(
                    &repo,
                    Some(base),
                    "README.md",
                    "New target",
                    "Changed target",
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            w.work(&safe_result(&w, risk, evidence)),
            AttemptState::Valid
        );
        assert_eq!(
            task_state(&w, &id),
            if reason == "checks_pending" {
                TaskState::WaitingForCi
            } else if reason == "head_changed" {
                TaskState::Closed
            } else if reason == "target_changed" {
                TaskState::Done
            } else {
                TaskState::NeedsHuman
            },
            "{reason}"
        );
        let s = f.state.lock().unwrap();
        assert_eq!(s.merged, reason == "target_changed", "{reason}");
        assert!(
            s.requests.iter().all(|r| reason == "target_changed"
                || r.method == "GET"
                || r.path.starts_with("/app/")),
            "{reason}"
        );
    }
}
#[test]
fn safe_delivery_does_not_treat_risk_context_as_a_second_veto() {
    for (risk, evidence) in [(Some("medium"), true), (None, false)] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        assert_eq!(
            w.work(&safe_result(&w, risk, evidence)),
            AttemptState::Valid
        );
        assert_eq!(task_state(&w, &id), TaskState::Done);
        let s = f.state.lock().unwrap();
        assert!(s.merged);
        assert_eq!(s.reviews[0]["state"], "APPROVED");
    }
}
#[test]
fn safe_delivery_recovers_lost_responses_and_external_automerge_without_duplicate_effects() {
    for loss in ["approval", "merge", "external", "external_lost_approval"] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        {
            let mut s = f.state.lock().unwrap();
            s.lose_review_response = loss == "approval" || loss == "external_lost_approval";
            s.lose_merge_response = loss == "merge";
            s.merge_on_approval = loss.starts_with("external");
        }
        assert_eq!(
            w.work(&safe_result(&w, Some("low"), true)),
            AttemptState::Valid
        );
        if loss != "external" {
            assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
            reopen(&mut w, &f);
            w.orchestrator.retry_publication(&id, "human").unwrap();
        }
        assert_eq!(task_state(&w, &id), TaskState::Done, "{loss}");
        let s = f.state.lock().unwrap();
        assert!(s.merged);
        assert_eq!(s.reviews.len(), 1);
        assert_eq!(
            s.requests.iter().filter(|r| r.method == "PUT").count(),
            usize::from(!loss.starts_with("external"))
        );
        assert_eq!(
            s.requests
                .iter()
                .filter(|r| r.method == "POST" && r.path.ends_with("/reviews"))
                .count(),
            1
        );
    }
}
#[test]
fn merge_uses_expected_head_and_never_bypasses_forge_rejection() {
    for reason in ["race", "protected"] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        if reason == "race" {
            f.state.lock().unwrap().move_head_on_merge = true;
        } else {
            f.state.lock().unwrap().overrides.insert(
                "/repos/org/app/pulls/33/merge".into(),
                Reply::new(405, json!({"message":"Required checks have not passed"})),
            );
        }
        assert_eq!(
            w.work(&safe_result(&w, Some("low"), true)),
            AttemptState::Valid
        );
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        assert!(!f.state.lock().unwrap().merged);
        assert!(w.orchestrator.retry_publication(&id, "human").is_err());
        assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
    }
}
#[test]
fn adaptation_under_merge_policy_remains_a_human_reviewed_proposal() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    let s = f.state.lock().unwrap();
    assert_eq!(s.proposals.len(), 1);
    assert_eq!(s.reviews[0]["state"], "COMMENTED");
    assert!(!s.merged);
}

#[test]
fn historical_comment_policy_digest_is_preserved_and_cannot_gain_approval() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    let task = w.orchestrator.db.get_task(&id).unwrap();
    let policy: artidezi_config::ResolvedProjectPolicy =
        serde_json::from_str(&task.policy_snapshot).unwrap();
    assert!(!task.policy_snapshot.contains("safe_verdict_action"));
    assert_eq!(task.policy_digest, policy.digest());
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .safe_verdict_action = artidezi_config::model::SafeVerdictAction::Approve;
    reopen(&mut w, &f);
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    no_publication(&f);
}

#[test]
fn an_edited_or_dismissed_approval_is_not_recreated_during_retry() {
    for changed in ["body", "dismissed", "owner"] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().lose_review_response = true;
        assert_eq!(
            w.work(&safe_result(&w, Some("low"), true)),
            AttemptState::Valid
        );
        {
            let mut s = f.state.lock().unwrap();
            match changed {
                "body" => s.reviews[0]["body"] = json!("Edited review"),
                "dismissed" => s.reviews[0]["state"] = json!("DISMISSED"),
                "owner" => s.reviews[0]["user"]["login"] = json!("another-bot[bot]"),
                _ => unreachable!(),
            }
        }
        reopen(&mut w, &f);
        assert!(
            w.orchestrator.retry_publication(&id, "human").is_err(),
            "{changed}"
        );
        let s = f.state.lock().unwrap();
        assert!(!s.merged);
        assert_eq!(s.reviews.len(), 1, "{changed}");
    }
}
