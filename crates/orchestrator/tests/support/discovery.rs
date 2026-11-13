// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use super::*;
use artidezi_config::model::{PullRequestAction, PullRequestDiscovery, PullRequestHandler};
use artidezi_forge::{MergeMethod, NewMergeRequest};
use artidezi_orchestrator::discovery::source_of;
use chrono::{Duration, Utc};

fn enable(w: &mut World) {
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests = Some(PullRequestDiscovery {
        poll_interval_seconds: 60,
        settle_seconds: 0,
        max_runs_per_pull_request: 2,
        max_active_tasks: 1,
        repositories: vec!["role".into()],
        ci_log_context: Default::default(),
        handlers: vec![PullRequestHandler {
            adaptation_action: Default::default(),
            safe_verdict_action: Default::default(),
            mode: Default::default(),
            id: "renovate".into(),
            agent_profile: None,
            authors: Some(vec!["renovate[bot]".into()]),
            author_regex: None,
            title_regex: None,
            catch_all: false,
            action: PullRequestAction::Assess,
            wait_for_checks_to_finish: false,
            require_successful_checks: false,
            ci_context: Default::default(),
            instructions: "Assess the update and preserve compatibility.".into(),
        }],
    });
}

fn head(w: &World, branch: &str, version: &str) -> artidezi_contract::common::CommitSha {
    let repo = w.forge.ensure_repository("org/role").unwrap();
    let main = repo.refname_to_id("refs/heads/main").unwrap();
    let parent = repo
        .refname_to_id(&format!("refs/heads/{branch}"))
        .unwrap_or(main);
    repo.reference("refs/heads/main", parent, true, "test commit parent")
        .unwrap();
    let next = seed(
        &repo,
        Some(parent),
        "defaults.yml",
        version,
        "Update dependency",
    );
    repo.reference("refs/heads/main", main, true, "restore test target")
        .unwrap();
    repo.reference(&format!("refs/heads/{branch}"), next, true, "input change")
        .unwrap();
    artidezi_contract::common::CommitSha(next.to_string())
}

fn open(w: &World, author: &str, branch: &str) -> artidezi_forge::MergeRequestInfo {
    head(w, branch, "version: 1.1.0\n");
    w.forge
        .open_merge_request_as(
            "org/role",
            author,
            &NewMergeRequest {
                title: "Update dependency".into(),
                body: "An upstream update".into(),
                source_branch: branch.into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap()
}

fn scan(w: &mut World) -> Vec<String> {
    let report = w
        .orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report.queued
}

fn approve(w: &World) -> PathBuf {
    w.script("approve.sh", r#"
test "$(git show refs/artidezi/inputs/source-pr:defaults.yml)" != "$(cat defaults.yml)"
cat > "$ARTIDEZI_RESULT_PATH" <<'JSON'
{"schema_version":1,"outcome":"approve_untouched","summary":"The proposed input is acceptable.","repositories":[{"id":"role","changed":false}]}
JSON
"#)
}

#[test]
fn known_source_rechecks_only_when_settled_and_all_checks_finish() {
    let mut w = World::new();
    enable(&mut w);
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.poll_interval_seconds = 600;
    discovery.settle_seconds = 120;
    discovery.handlers[0].wait_for_checks_to_finish = true;
    open(&w, "renovate[bot]", "renovate/slow-ci");
    w.forge.set_checks(
        "org/role",
        "renovate/slow-ci",
        artidezi_forge::ChecksState::Pending,
    );
    let start = Utc::now();
    let first = w
        .orchestrator
        .scan_pull_requests_at(Some("roles"), true, start)
        .unwrap();
    assert!(first.queued.is_empty());
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert_eq!(source.follow_up.unwrap().at, start + Duration::seconds(120));

    assert!(
        w.orchestrator
            .tick_at(start + Duration::seconds(119))
            .unwrap()
            .discovered
            .is_empty()
    );
    assert!(
        w.orchestrator
            .tick_at(start + Duration::seconds(120))
            .unwrap()
            .discovered
            .is_empty()
    );
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert_eq!(source.follow_up.unwrap().at, start + Duration::seconds(240));

    w.forge.set_checks(
        "org/role",
        "renovate/slow-ci",
        artidezi_forge::ChecksState::Failure,
    );
    assert!(
        w.orchestrator
            .tick_at(start + Duration::seconds(239))
            .unwrap()
            .discovered
            .is_empty()
    );
    let next = w
        .orchestrator
        .tick_at(start + Duration::seconds(240))
        .unwrap();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.discovered.len(), 1);
    assert!(
        w.orchestrator.db.list_pull_request_sources(None).unwrap()[0]
            .follow_up
            .is_none()
    );
}

#[test]
fn no_ci_queues_at_the_settle_deadline() {
    let mut w = World::new();
    enable(&mut w);
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.poll_interval_seconds = 600;
    discovery.settle_seconds = 120;
    discovery.handlers[0].wait_for_checks_to_finish = true;
    open(&w, "renovate[bot]", "renovate/no-ci");
    w.forge.set_checks(
        "org/role",
        "renovate/no-ci",
        artidezi_forge::ChecksState::Absent,
    );
    let start = Utc::now();
    assert!(
        w.orchestrator
            .scan_pull_requests_at(Some("roles"), true, start)
            .unwrap()
            .queued
            .is_empty()
    );
    let next = w
        .orchestrator
        .tick_at(start + Duration::seconds(120))
        .unwrap();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.discovered.len(), 1);
}

#[test]
fn direct_follow_up_restarts_settling_when_the_head_moves() {
    let mut w = World::new();
    enable(&mut w);
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.poll_interval_seconds = 600;
    discovery.settle_seconds = 120;
    let first = open(&w, "renovate[bot]", "renovate/moving-head");
    w.forge.set_checks(
        "org/role",
        "renovate/moving-head",
        artidezi_forge::ChecksState::Absent,
    );
    let start = Utc::now();
    assert!(
        w.orchestrator
            .scan_pull_requests_at(Some("roles"), true, start)
            .unwrap()
            .queued
            .is_empty()
    );
    let newest = head(&w, "renovate/moving-head", "version: 1.2.0\n");
    assert_ne!(first.head_commit, newest);
    assert!(
        w.orchestrator
            .tick_at(start + Duration::seconds(120))
            .unwrap()
            .discovered
            .is_empty()
    );
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert_eq!(source.observed_head, newest.0);
    assert_eq!(source.follow_up.unwrap().at, start + Duration::seconds(240));
    let next = w
        .orchestrator
        .tick_at(start + Duration::seconds(240))
        .unwrap();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.discovered.len(), 1);
    let task = w.orchestrator.db.get_task(&next.discovered[0]).unwrap();
    assert_eq!(source_of(&task).unwrap().unwrap().input.commit, newest);
}

#[test]
fn scoped_scan_does_not_retire_another_projects_removed_source() {
    let mut w = World::new();
    enable(&mut w);
    open(&w, "renovate[bot]", "renovate/removed-project");
    let task_id = scan(&mut w).pop().unwrap();
    let source_id = w
        .orchestrator
        .db
        .list_pull_request_sources(Some("roles"))
        .unwrap()[0]
        .id
        .clone();
    let mut other = w.orchestrator.config.projects["roles"].clone();
    other.pull_requests = None;
    w.orchestrator.config.projects.insert("other".into(), other);
    w.orchestrator.config.projects.remove("roles");

    w.orchestrator
        .scan_pull_requests(Some("other"), true)
        .unwrap();
    assert!(
        !w.orchestrator
            .db
            .get_pull_request_source(&source_id)
            .unwrap()
            .closed
    );
    assert!(
        !w.orchestrator
            .db
            .get_task(&task_id)
            .unwrap()
            .state
            .is_terminal()
    );

    w.orchestrator.scan_pull_requests(None, true).unwrap();
    assert!(
        w.orchestrator
            .db
            .get_pull_request_source(&source_id)
            .unwrap()
            .closed
    );
    assert!(
        w.orchestrator
            .db
            .get_task(&task_id)
            .unwrap()
            .state
            .is_terminal()
    );
}

#[test]
fn opted_in_ci_results_are_frozen_and_rendered_as_untrusted_context() {
    let mut w = World::new();
    enable(&mut w);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .ci_context
        .results = true;
    let pr = open(&w, "renovate[bot]", "renovate/ci-context");
    w.forge.set_checks(
        "org/role",
        "renovate/ci-context",
        artidezi_forge::ChecksState::Success,
    );

    let task_id = scan(&mut w).pop().unwrap();
    let task = w.orchestrator.db.get_task(&task_id).unwrap();
    let source = source_of(&task).unwrap().unwrap();
    let snapshot = source.ci_snapshot.as_ref().unwrap();
    assert_eq!(
        snapshot.overall,
        artidezi_config::resolve::PullRequestCiState::Success
    );
    assert_eq!(snapshot.results.len(), 1);
    assert_eq!(snapshot.results[0].name, "fake/ci");
    assert_eq!(snapshot.results[0].status, "success");
    assert_eq!(source.input.commit, pr.head_commit);

    let attempt = w.orchestrator.db.latest_attempt(&task_id).unwrap().unwrap();
    let job = w.orchestrator.load_job(&attempt).unwrap();
    assert_eq!(job.prompt_template_version, 12);
    let prompt =
        String::from_utf8(w.orchestrator.artifacts.get(&job.task_document).unwrap()).unwrap();
    let handler = prompt
        .find("## Handler instructions (trusted configuration)")
        .unwrap();
    assert!(handler < prompt.find("## TASK (untrusted input)").unwrap());
    assert!(prompt.contains("## Source CI snapshot (untrusted forge data)"));
    assert!(prompt.contains("\"overall\": \"success\""));
    assert!(prompt.contains("\"name\": \"fake/ci\""));
    assert!(!prompt.contains("Selected source CI logs"));
}

#[test]
fn opt_in_matching_and_persistent_exact_head_assessment() {
    let mut w = World::new();
    let pr = open(&w, "renovate[bot]", "renovate/update");
    assert!(scan(&mut w).is_empty());
    enable(&mut w);
    open(&w, "someone-else", "feature/ignored");
    open(&w, "artidezi[bot]", "artidezi/own");
    let tasks = scan(&mut w);
    assert_eq!(tasks.len(), 1);
    let task = w.orchestrator.db.get_task(&tasks[0]).unwrap();
    assert_eq!(task.delivery_mode, DeliveryMode::Shadow);
    assert_eq!(
        source_of(&task).unwrap().unwrap().input.commit,
        pr.head_commit
    );
    let script = approve(&w);
    assert_eq!(w.work(&script), AttemptState::Valid);
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
    assert!(scan(&mut w).is_empty());
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert("fake".into(), Box::new(w.forge.clone()));
    w.orchestrator =
        Orchestrator::open(w.orchestrator.config.clone(), &w.root.join("data"), forges).unwrap();
    assert!(
        scan(&mut w).is_empty(),
        "restart must not repeat the model call"
    );
    w.forge
        .update_merge_request(
            "org/role",
            pr.number,
            "Edited title",
            "A new comment-like description",
        )
        .unwrap();
    assert!(
        scan(&mut w).is_empty(),
        "metadata updates are not activations"
    );
    assert_eq!(
        w.orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn handler_profile_overrides_the_project_default_and_is_frozen() {
    let mut w = World::new();
    let profile = w.orchestrator.config.agent_profiles["fake"].clone();
    w.orchestrator
        .config
        .agent_profiles
        .insert("fake-low".into(), profile);
    let project = w.orchestrator.config.projects.get_mut("roles").unwrap();
    project.allowed_agent_profiles.push("fake-low".into());
    enable(&mut w);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .agent_profile = Some("fake-low".into());

    open(&w, "renovate[bot]", "renovate/profile");
    let task_id = scan(&mut w).remove(0);
    let task = w.orchestrator.db.get_task(&task_id).unwrap();
    assert_eq!(task.agent_profile, "fake-low");
    let policy: artidezi_config::resolve::ResolvedProjectPolicy =
        serde_json::from_str(&task.policy_snapshot).unwrap();
    assert_eq!(policy.agent_profile_name, "fake-low");

    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .agent_profile = None;
    assert_eq!(
        w.orchestrator.db.get_task(&task_id).unwrap().agent_profile,
        "fake-low",
        "existing source tasks retain their resolved profile"
    );
}

#[test]
fn title_scoped_handlers_select_only_matching_changes_and_freeze_the_title() {
    let mut w = World::new();
    enable(&mut w);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .title_regex = Some(r"^Update selected dependency to v[0-9]+$".into());
    let selected = open(&w, "renovate[bot]", "renovate/selected");
    w.forge
        .update_merge_request(
            "org/role",
            selected.number,
            "Update selected dependency to v2",
            "Selected update",
        )
        .unwrap();
    open(&w, "renovate[bot]", "renovate/unmatched");

    let tasks = scan(&mut w);
    assert_eq!(tasks.len(), 1);
    let task = w.orchestrator.db.get_task(&tasks[0]).unwrap();
    let source = source_of(&task).unwrap().unwrap();
    assert_eq!(source.number, selected.number);
    assert_eq!(
        source.title.as_deref(),
        Some("Update selected dependency to v2")
    );
    assert_eq!(
        w.orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .len(),
        1,
        "an unmatched PR must not become tracked work"
    );

    w.forge
        .update_merge_request(
            "org/role",
            selected.number,
            "Update another dependency to v2",
            "Retitled outside the authorized rule",
        )
        .unwrap();
    let script = approve(&w);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::Closed,
        "a title change must obsolete the scoped assessment"
    );
}

#[test]
fn first_matching_handler_does_not_fall_through_while_waiting_for_checks() {
    let mut w = World::new();
    enable(&mut w);
    let discovery = w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.handlers[0].require_successful_checks = true;
    discovery.handlers[0].wait_for_checks_to_finish = true;
    discovery.handlers.push(PullRequestHandler {
        adaptation_action: Default::default(),
        safe_verdict_action: Default::default(),
        mode: Default::default(),
        id: "fallback".into(),
        agent_profile: None,
        authors: None,
        author_regex: None,
        title_regex: None,
        catch_all: true,
        action: PullRequestAction::Assess,
        wait_for_checks_to_finish: false,
        require_successful_checks: false,
        ci_context: Default::default(),
        instructions: "Other input".into(),
    });
    open(&w, "renovate[bot]", "renovate/update");
    let other = open(&w, "human", "feature/human");
    let tasks = scan(&mut w);
    assert_eq!(tasks.len(), 1);
    let task = w.orchestrator.db.get_task(&tasks[0]).unwrap();
    let source = source_of(&task).unwrap().unwrap();
    assert_eq!(source.handler_id, "fallback");
    assert_eq!(source.number, other.number);
    let script = approve(&w);
    w.work(&script);
    w.forge
        .set_checks("org/role", "renovate/update", ChecksState::Success);
    let next = scan(&mut w);
    assert_eq!(next.len(), 1);
    assert_eq!(
        source_of(&w.orchestrator.db.get_task(&next[0]).unwrap())
            .unwrap()
            .unwrap()
            .handler_id,
        "renovate"
    );
}

#[test]
fn failed_checks_are_assessed_for_possible_adaptation() {
    let mut w = World::new();
    enable(&mut w);
    open(&w, "renovate[bot]", "renovate/update");
    w.forge
        .set_checks("org/role", "renovate/update", ChecksState::Failure);
    let tasks = scan(&mut w);
    assert_eq!(tasks.len(), 1);
}

#[test]
fn shadow_proposal_is_verified_but_never_published() {
    let mut w = World::new();
    enable(&mut w);
    let pr = open(&w, "renovate[bot]", "renovate/update");
    let target = w.forge.branch_head("org/role", "main").unwrap();
    let task = scan(&mut w).remove(0);
    let script = w.script("adapt.sh", r#"
git cherry-pick refs/artidezi/inputs/source-pr
echo adapted > CHANGES.md
git add CHANGES.md
git commit -m 'Adapt the update'
sha=$(git rev-parse HEAD)
printf '{"schema_version":1,"outcome":"proposal","summary":"Adapted the change.","repositories":[{"id":"role","changed":true,"head_commit":"%s"}]}' "$sha" > "$ARTIDEZI_RESULT_PATH"
"#);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task).unwrap().state,
        TaskState::Done
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        w.forge
            .list_open_merge_requests("org/role", None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(w.forge.branch_head("org/role", "main").unwrap(), target);
    assert_eq!(
        w.forge
            .get_merge_request("org/role", pr.number)
            .unwrap()
            .head_commit,
        pr.head_commit
    );
    let attempt = w.orchestrator.db.latest_attempt(&task).unwrap().unwrap();
    let record: artidezi_contract::attempt::AttemptRecord = w
        .orchestrator
        .artifacts
        .get_json(&attempt.attempt_record.unwrap())
        .unwrap();
    assert_eq!(
        record
            .repositories
            .iter()
            .find(|r| r.id.as_str() == "role")
            .unwrap()
            .verification[0]
            .exit_code,
        Some(0)
    );
}

#[test]
fn changed_head_suppresses_obsolete_verdict_and_enforces_persistent_budget() {
    let mut w = World::new();
    enable(&mut w);
    open(&w, "renovate[bot]", "renovate/update");
    let first = scan(&mut w).remove(0);
    let script = approve(&w);
    let forge = w.forge.clone();
    assert_eq!(
        w.work_with_record(&script, |_| {
            // Move the PR after worker completion but before controller ingestion.
            let repo = forge.ensure_repository("org/role").unwrap();
            let base = repo.refname_to_id("refs/heads/main").unwrap();
            let next = seed(
                &repo,
                Some(base),
                "defaults.yml",
                "version: 1.2.0\n",
                "New revision",
            );
            repo.reference("refs/heads/main", base, true, "restore")
                .unwrap();
            repo.reference("refs/heads/renovate/update", next, true, "rebase")
                .unwrap();
        }),
        AttemptState::Valid
    );
    assert_eq!(
        w.orchestrator.db.get_task(&first).unwrap().state,
        TaskState::Closed
    );
    let second = scan(&mut w).remove(0);
    w.work(&script);
    assert_eq!(
        w.orchestrator.db.get_task(&second).unwrap().state,
        TaskState::Done
    );
    head(&w, "renovate/update", "version: 1.3.0\n");
    assert!(scan(&mut w).is_empty());
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert!(source.paused);
    assert_eq!(source.runs, 2);
    assert!(scan(&mut w).is_empty());
    w.orchestrator
        .resume_pull_request(&source.id, "test-admin")
        .unwrap();
    assert_eq!(scan(&mut w).len(), 1);
}

#[test]
fn human_hold_survives_head_changes_and_revision_uses_current_input() {
    let mut w = World::new();
    enable(&mut w);
    open(&w, "renovate[bot]", "renovate/update");
    let first = scan(&mut w).remove(0);
    let script = w.script("question.sh", r#"cat > "$ARTIDEZI_RESULT_PATH" <<'JSON'
{"schema_version":1,"outcome":"needs_human","summary":"A choice is needed.","repositories":[{"id":"role","changed":false}],"questions_for_human":["Which behavior is intended?"]}
JSON
"#);
    w.work(&script);
    let newer = head(&w, "renovate/update", "version: 2.0.0\n");
    assert!(scan(&mut w).is_empty());
    let next = w
        .orchestrator
        .request_revision(&first, "Use the new default.", "admin")
        .unwrap();
    assert_ne!(next.task_id, first);
    let task = w.orchestrator.db.get_task(&next.task_id).unwrap();
    assert_eq!(source_of(&task).unwrap().unwrap().input.commit, newer);
    assert_eq!(task.delivery_mode, DeliveryMode::Shadow);
    assert!(
        w.orchestrator
            .request_revision(&first, "Retry stale task", "admin")
            .is_err()
    );
}

#[test]
fn source_closure_stops_queued_work_and_timing_is_persistent() {
    let mut w = World::new();
    enable(&mut w);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .settle_seconds = 10;
    let pr = open(&w, "renovate[bot]", "renovate/update");
    let now = Utc::now();
    assert!(
        w.orchestrator
            .scan_pull_requests_at(None, false, now)
            .unwrap()
            .queued
            .is_empty()
    );
    assert!(
        w.orchestrator
            .scan_pull_requests_at(None, false, now + Duration::seconds(30))
            .unwrap()
            .queued
            .is_empty()
    );
    let tasks = w
        .orchestrator
        .scan_pull_requests_at(None, false, now + Duration::seconds(61))
        .unwrap()
        .queued;
    assert_eq!(tasks.len(), 1);
    w.forge
        .merge("org/role", pr.number, &pr.head_commit, MergeMethod::Merge)
        .unwrap();
    assert!(scan(&mut w).is_empty());
    assert_eq!(
        w.orchestrator.db.get_task(&tasks[0]).unwrap().state,
        TaskState::Closed
    );
    assert!(
        w.orchestrator
            .lease("personal", "w", Duration::minutes(5))
            .unwrap()
            .is_none()
    );
}

#[test]
fn source_merge_cancels_leased_work_even_when_evaluations_are_paused() {
    for paused in [false, true] {
        let mut w = World::new();
        enable(&mut w);
        let pr = open(&w, "renovate[bot]", "renovate/update");
        let task = scan(&mut w).remove(0);
        let work = w
            .orchestrator
            .lease("personal", "worker", Duration::minutes(5))
            .unwrap()
            .unwrap();
        if paused {
            let source = source_of(&w.orchestrator.db.get_task(&task).unwrap())
                .unwrap()
                .unwrap();
            w.orchestrator
                .pause_pull_request(&source.tracking_id, "admin")
                .unwrap();
            assert_eq!(
                w.orchestrator
                    .db
                    .get_attempt(&work.attempt.id)
                    .unwrap()
                    .state,
                AttemptState::Leased
            );
        }
        w.forge
            .merge("org/role", pr.number, &pr.head_commit, MergeMethod::Merge)
            .unwrap();
        assert!(scan(&mut w).is_empty());
        assert_eq!(
            w.orchestrator.db.get_task(&task).unwrap().state,
            TaskState::Closed
        );
        assert_eq!(
            w.orchestrator
                .db
                .get_attempt(&work.attempt.id)
                .unwrap()
                .state,
            AttemptState::Cancelled
        );
        assert!(
            w.orchestrator
                .db
                .get_lease(&work.attempt.id)
                .unwrap()
                .is_none()
        );
        assert!(
            w.orchestrator
                .db
                .list_merge_requests(&task)
                .unwrap()
                .is_empty()
        );
        assert!(scan(&mut w).is_empty(), "closure is idempotent");
    }
}

#[test]
fn explicit_pause_stops_queued_work_and_blocks_new_heads_until_resume() {
    let mut w = World::new();
    enable(&mut w);
    open(&w, "renovate[bot]", "renovate/update");
    let task = scan(&mut w).remove(0);
    let source = source_of(&w.orchestrator.db.get_task(&task).unwrap())
        .unwrap()
        .unwrap();
    w.orchestrator
        .pause_pull_request(&source.tracking_id, "admin")
        .unwrap();
    assert_eq!(
        w.orchestrator
            .db
            .latest_attempt(&task)
            .unwrap()
            .unwrap()
            .state,
        AttemptState::Cancelled
    );
    assert!(
        w.orchestrator
            .lease("personal", "worker", Duration::minutes(1))
            .unwrap()
            .is_none()
    );
    head(&w, "renovate/update", "version: 1.4.0\n");
    assert!(scan(&mut w).is_empty());
    w.orchestrator
        .resume_pull_request(&source.tracking_id, "admin")
        .unwrap();
    assert_eq!(scan(&mut w).len(), 1);
}

#[test]
fn source_adaptation_publishes_two_proposals_and_observes_only_human_merges() {
    let mut w = World::new();
    enable(&mut w);
    let project = w.orchestrator.config.projects.get_mut("roles").unwrap();
    project.allowed_delivery_modes = vec![DeliveryMode::MergeRequest];
    project.pull_requests.as_mut().unwrap().handlers[0].mode =
        artidezi_config::model::PullRequestMode::Publish;
    project
        .repositories
        .get_mut("playbook")
        .unwrap()
        .agent_access = artidezi_contract::common::RepositoryAccess::ReadWrite;
    let source = open(&w, "renovate[bot]", "renovate/update");
    let task = scan(&mut w).remove(0);
    let script=w.script("two-proposals.sh",r#"
cd "$ARTIDEZI_WORKSPACE/repos/role"
git checkout refs/artidezi/inputs/source-pr -- defaults.yml
echo 'changed' > CHANGES.md
git add defaults.yml CHANGES.md
git commit -qm 'Adapt the update'
role=$(git rev-parse HEAD)
cd ../playbook
echo 'adapted consumer' >> README.md
git add README.md
git commit -qm 'Adapt the consumer'
consumer=$(git rev-parse HEAD)
cat > "$ARTIDEZI_RESULT_PATH" <<JSON
{"schema_version":1,"outcome":"proposal","summary":"Prepared component and consumer adaptations.","repositories":[{"id":"role","changed":true,"head_commit":"$role"},{"id":"playbook","changed":true,"head_commit":"$consumer"}]}
JSON
"#);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task).unwrap().state,
        TaskState::AwaitingReview
    );
    let proposals = w.orchestrator.db.list_merge_requests(&task).unwrap();
    assert_eq!(proposals.len(), 2);
    let reviews = w.forge.list_reviews("org/role", source.number).unwrap();
    assert_eq!(reviews.len(), 1);
    for p in &proposals {
        assert!(reviews[0].body.contains(&p.url));
    }
    assert_eq!(
        w.orchestrator.reconcile_task(&task).unwrap(),
        TaskState::AwaitingReview
    );
    assert_eq!(
        w.forge
            .get_merge_request("org/role", source.number)
            .unwrap()
            .state,
        artidezi_forge::MergeRequestState::Open
    );
    for (index, p) in proposals.iter().enumerate() {
        let remote = if p.repository_id == "role" {
            "org/role"
        } else {
            "org/playbook"
        };
        w.forge
            .merge(
                remote,
                p.number,
                &artidezi_contract::common::CommitSha(p.head_commit.clone()),
                MergeMethod::Merge,
            )
            .unwrap();
        assert_eq!(
            w.orchestrator.reconcile_task(&task).unwrap(),
            if index == 0 {
                TaskState::AwaitingReview
            } else {
                TaskState::Done
            }
        );
        for (other_index, other) in proposals.iter().enumerate() {
            let remote = if other.repository_id == "role" {
                "org/role"
            } else {
                "org/playbook"
            };
            assert_eq!(
                w.forge
                    .ensure_repository(remote)
                    .unwrap()
                    .find_reference(&format!("refs/heads/{}", other.source_branch))
                    .is_err(),
                other_index <= index
            );
        }
    }
    assert_eq!(
        w.orchestrator.reconcile_task(&task).unwrap(),
        TaskState::Done
    );
    assert!(
        w.forge
            .ensure_repository("org/role")
            .unwrap()
            .tag_names(None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retention_expires_pr_bundle_edges_in_jobs_and_frozen_source_policies() {
    let mut w = World::new();
    enable(&mut w);
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    open(&w, "renovate[bot]", "renovate/retention");
    let task_id = scan(&mut w).remove(0);
    let task = w.orchestrator.db.get_task(&task_id).unwrap();
    let source = source_of(&task).unwrap().unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&task_id).unwrap().unwrap();
    let job: artidezi_contract::Job = w.orchestrator.artifacts.get_json(&attempt.job).unwrap();
    assert!(
        job.repositories
            .iter()
            .flat_map(|repo| &repo.input_refs)
            .any(|input| input.bundle == source.input.bundle)
    );
    w.orchestrator.close_task(&task_id, "test").unwrap();
    assert!(
        w.orchestrator
            .cleanup_artifacts_at(Utc::now(), false)
            .unwrap()
            .candidates
            .is_empty()
    );
    let later = Utc::now() + Duration::days(2);
    let preview = w.orchestrator.cleanup_artifacts_at(later, false).unwrap();
    assert!(
        preview
            .candidates
            .iter()
            .any(|candidate| candidate.artifact == source.input.bundle)
    );
    let report = w.orchestrator.cleanup_artifacts_at(later, true).unwrap();
    assert_eq!(report.removed_bytes, preview.reclaimable_bytes);
    assert!(!w.orchestrator.artifacts.contains(&source.input.bundle));
    assert_eq!(
        source_of(&w.orchestrator.db.get_task(&task_id).unwrap())
            .unwrap()
            .unwrap(),
        source
    );
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(later, true)
            .unwrap()
            .removed_bytes,
        0
    );
}
