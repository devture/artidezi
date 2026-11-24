// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Evaluating held changes again: what earns it, what it costs, and what stays with a person.

use super::supersession::rebase_source;
use super::*;
use artidezi_orchestrator::discovery::source_of;

fn source(w: &World) -> TrackedPullRequest {
    w.orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0)
}
fn hold(w: &World) -> Hold {
    source(w).hold.expect("held source")
}
fn retries(w: &mut World) -> &mut artidezi_config::model::Retries {
    &mut discovery(w).retries
}
fn discovery(w: &mut World) -> &mut PullRequestDiscovery {
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
}
/// A pass that long from now, after discovery had a look.
fn tick(w: &mut World, seconds: i64) -> artidezi_orchestrator::maintenance::TickReport {
    let report = w
        .orchestrator
        .tick_at(Timestamp::now() + SignedDuration::from_secs(seconds))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}
/// Enough passes to see a change, decide about it and queue its evaluation, without waiting.
fn settle(w: &mut World) -> Vec<String> {
    let mut queued = scan(w);
    for _ in 0..2 {
        queued.extend(tick(w, 5).discovered);
    }
    queued
}
fn question(w: &World) -> PathBuf {
    w.script("question.sh", &result_writer(r#"{"schema_version":1,"outcome":"needs_human","summary":"Maintainer decision required.","questions_for_human":["Which behavior should be preserved?"],"repositories":[{"id":"role","changed":false}]}"#))
}
fn failed(w: &World) -> PathBuf {
    w.script("failed.sh", &result_writer(r#"{"schema_version":1,"outcome":"failed","summary":"Assessment failed.","repositories":[{"id":"role","changed":false}]}"#))
}
fn blocked(w: &World) -> PathBuf {
    w.script("blocked.sh", &result_writer(r#"{"schema_version":1,"outcome":"retry_later","summary":"Not decidable yet.","retry_reason":"The image 1.2.3\nis not published yet","retry_hint":"soon","repositories":[{"id":"role","changed":false}]}"#))
}
fn slowly_blocked(w: &World) -> PathBuf {
    w.script("slowly_blocked.sh", &result_writer(r#"{"schema_version":1,"outcome":"retry_later","summary":"Not decidable yet.","retry_reason":"The merged fix is not released yet","retry_hint":"slow","repositories":[{"id":"role","changed":false}]}"#))
}
fn prompt(w: &World, id: &str) -> String {
    let attempt = w.orchestrator.db.latest_attempt(id).unwrap().unwrap();
    let job = w.orchestrator.load_job(&attempt).unwrap();
    assert_eq!(job.schema_version, 4);
    String::from_utf8(w.orchestrator.artifacts.get(&job.task_document).unwrap()).unwrap()
}
/// A published assessment that asks for a decision.
fn asking() -> (World, Fixture, String) {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = question(&w);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(hold(&w).reason, HoldReason::Question);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
    (w, f, id)
}
fn advance_target(w: &World) -> Oid {
    let repo = w.forge.ensure_repository("org/app").unwrap();
    let base = repo.refname_to_id("refs/heads/main").unwrap();
    seed(
        &repo,
        Some(base),
        "other.txt",
        &format!("after {base}\n"),
        "Advance target",
    )
}
fn event(w: &World, id: &str, kind: &str) -> serde_json::Value {
    w.orchestrator
        .db
        .list_events(id)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == kind)
        .unwrap()
        .payload
}
/// Store the hold as a release without typed holds did.
fn untype(w: &World, reason: Option<&str>, id: &str) {
    let mut tracked = source(w);
    tracked.hold = None;
    tracked.paused = true;
    tracked.reevaluations = Default::default();
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    if let Some(reason) = reason {
        w.orchestrator
            .db
            .record_event(
                Some(id),
                None,
                "orchestrator",
                "task.needs_human",
                json!({"reason": reason}),
            )
            .unwrap();
    }
}

#[test]
fn a_question_is_reassessed_when_the_change_itself_differs_within_the_budget() {
    let (mut w, f, id) = asking();
    assert!(settle(&mut w).is_empty(), "nothing moved");
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let head = move_source_head(&w, &f);
    let queued = settle(&mut w);
    assert_eq!(queued.len(), 1);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    let tracked = source(&w);
    assert_eq!(tracked.runs, 2);
    assert_eq!(tracked.reevaluations, Default::default());
    let task = w.orchestrator.db.get_task(&queued[0]).unwrap();
    assert_eq!(source_of(&task).unwrap().unwrap().input.commit, head);
    let script = safe_result(&w, Some("low"), true);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &queued[0]), TaskState::Done);
    assert!(f.state.lock().unwrap().merged);

    // An exhausted budget keeps the question.
    let (mut w, f, id) = asking();
    discovery(&mut w).max_runs_per_pull_request = 1;
    move_source_head(&w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(hold(&w).reason, HoldReason::Question);
}

#[test]
fn a_moved_target_or_mere_rebase_earns_one_delayed_evaluation_outside_the_budget() {
    for rebase in [false, true] {
        let (mut w, f, id) = asking();
        discovery(&mut w).max_runs_per_pull_request = 1;
        if rebase {
            rebase_source(&w, &f);
        } else {
            advance_target(&w);
        }
        assert!(settle(&mut w).is_empty(), "not right away");
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        let held = hold(&w);
        assert_eq!(held.retry, Some(EvaluationRetry::TargetChange));
        assert_eq!((held.retries, held.max_retries), (1, 1));
        assert_eq!(held.rebased_head.is_some(), rebase);
        let wait = held
            .next_at
            .unwrap()
            .duration_since(Timestamp::now())
            .as_secs();
        assert!((3500..=3610).contains(&wait), "{wait}");
        // Further looks do not put it off.
        assert!(settle(&mut w).is_empty());
        assert_eq!(hold(&w).next_at, held.next_at);

        reopen(&mut w, &f);
        let queued = tick(&mut w, 3700).discovered;
        assert_eq!(queued.len(), 1, "rebase: {rebase}");
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        let tracked = source(&w);
        assert_eq!(tracked.runs, 1, "outside the budget");
        assert_eq!(tracked.reevaluations.target_change, 1);
        let task = w.orchestrator.db.get_task(&queued[0]).unwrap();
        let repo = w.forge.ensure_repository("org/app").unwrap();
        assert_eq!(
            task.repositories
                .iter()
                .find(|repository| repository.repository_id == "role")
                .unwrap()
                .base_commit,
            repo.refname_to_id("refs/heads/main").unwrap().to_string()
        );

        // Only once per change.
        let script = question(&w);
        assert_eq!(w.work(&script), AttemptState::Valid);
        advance_target(&w);
        assert!(settle(&mut w).is_empty());
        assert_eq!(hold(&w).retry, None);
        assert!(tick(&mut w, 90_000).discovered.is_empty());
        assert_eq!(task_state(&w, &queued[0]), TaskState::NeedsHuman);

        // A different change starts over.
        discovery(&mut w).max_runs_per_pull_request = 3;
        move_source_head(&w, &f);
        // The blocked look is repeated after a while.
        let queued = tick(&mut w, 100_000).discovered;
        let queued = if queued.is_empty() {
            tick(&mut w, 110_000).discovered
        } else {
            queued
        };
        assert_eq!(queued.len(), 1);
        assert_eq!(source(&w).runs, 2);
        assert_eq!(source(&w).reevaluations, Default::default());
    }
}

#[test]
fn an_undone_target_move_or_switched_off_retry_does_not_evaluate() {
    for case in ["undone", "off"] {
        let (mut w, f, id) = asking();
        let repo = w.forge.ensure_repository("org/app").unwrap();
        let base = repo.refname_to_id("refs/heads/main").unwrap();
        if case == "off" {
            retries(&mut w).target_change.max = 0;
        }
        advance_target(&w);
        assert!(settle(&mut w).is_empty());
        if case == "undone" {
            assert!(hold(&w).next_at.is_some());
            repo.reference("refs/heads/main", base, true, "undo")
                .unwrap();
        } else {
            assert_eq!(hold(&w).next_at, None);
        }
        reopen(&mut w, &f);
        assert!(tick(&mut w, 3700).discovered.is_empty(), "{case}");
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        assert_eq!(hold(&w).next_at, None, "{case}");
    }
}

#[test]
fn a_person_s_decision_keeps_a_question_held() {
    for case in ["objection", "dismissed", "edited", "deleted", "pause"] {
        for moved in ["head", "target"] {
            let (mut w, f, id) = asking();
            if moved == "head" {
                move_source_head(&w, &f);
            } else {
                advance_target(&w);
            }
            {
                let mut state = f.state.lock().unwrap();
                match case {
                    "objection" => {
                        let mut review = state.reviews[0].clone();
                        review["id"] = json!(900);
                        review["user"]["login"] = json!("maintainer");
                        review["state"] = json!("CHANGES_REQUESTED");
                        review["body"] = json!("No.");
                        state.reviews.push(review);
                    }
                    "dismissed" => state.reviews[0]["state"] = json!("DISMISSED"),
                    "edited" => {
                        let body = state.reviews[0]["body"].as_str().unwrap().to_owned();
                        state.reviews[0]["body"] = json!(format!("{body}\nEdited."));
                    }
                    "deleted" => state.reviews.clear(),
                    _ => {}
                }
            }
            if case == "pause" {
                let tracked = source(&w);
                w.orchestrator
                    .pause_pull_request(&tracked.id, "operator")
                    .unwrap();
            }
            assert!(settle(&mut w).is_empty(), "{case} {moved}");
            assert!(tick(&mut w, 3700).discovered.is_empty(), "{case} {moved}");
            assert!(tick(&mut w, 9000).discovered.is_empty(), "{case} {moved}");
            assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{case} {moved}");
            assert!(source(&w).paused);
            if case == "objection" && moved == "head" {
                // Once it is withdrawn, the new head is assessed.
                f.state.lock().unwrap().reviews.pop();
                assert_eq!(tick(&mut w, 20_000).discovered.len(), 1);
            }
        }
    }
}

#[test]
fn a_failed_run_is_evaluated_once_more_and_then_needs_a_person() {
    for kind in ["outcome", "no_result", "shadow"] {
        let (mut w, f) = if kind == "shadow" {
            world()
        } else {
            safe_world(true)
        };
        let id = scan(&mut w).remove(0);
        let script = if kind == "no_result" {
            w.script("broken.sh", "exit 3")
        } else {
            failed(&w)
        };
        w.work(&script);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry, "{kind}");
        let held = hold(&w);
        assert_eq!(held.reason, HoldReason::FailedRun);
        assert_eq!(held.retry, Some(EvaluationRetry::Failed));
        assert!(!source(&w).paused);
        assert!(settle(&mut w).is_empty());
        assert_eq!(hold(&w).next_at, held.next_at);
        assert!(f.state.lock().unwrap().reviews.is_empty());

        reopen(&mut w, &f);
        let queued = tick(&mut w, 3700).discovered;
        assert_eq!(queued.len(), 1, "{kind}");
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        assert_eq!(source(&w).runs, 1);
        assert_eq!(source(&w).reevaluations.failed, 1);
        w.work(&script);
        assert_eq!(task_state(&w, &queued[0]), TaskState::NeedsHuman, "{kind}");
        assert!(tick(&mut w, 90_000).discovered.is_empty());
        assert_eq!(hold(&w).reason, HoldReason::FailedRun);
        // A moved target still earns its own.
        advance_target(&w);
        scan(&mut w);
        assert_eq!(tick(&mut w, 100_000).discovered.len(), 1, "{kind}");
        assert_eq!(source(&w).runs, 1);
    }
}

#[test]
fn a_failed_run_waiting_for_its_retry_follows_the_change() {
    // A different change does not wait.
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = failed(&w);
    w.work(&script);
    move_source_head(&w, &f);
    assert_eq!(settle(&mut w).len(), 1);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(source(&w).runs, 2);

    // A rebase does, and an objection ends the wait.
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    let at = hold(&w).next_at;
    rebase_source(&w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(hold(&w).next_at, at);
    assert_eq!(hold(&w).retry, Some(EvaluationRetry::Failed));
    f.state.lock().unwrap().reviews.push(json!({"id": 900, "user": {"login": "maintainer", "type": "User"}, "commit_id": source(&w).observed_head, "body": "No.", "state": "CHANGES_REQUESTED"}));
    assert!(tick(&mut w, 3700).discovered.is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);

    // A closed source ends it too, and a pause hands it to a person.
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    f.state.lock().unwrap().open = false;
    scan(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(source(&w).hold, None);

    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    let tracked = source(&w);
    w.orchestrator
        .pause_pull_request(&tracked.id, "operator")
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(tick(&mut w, 90_000).discovered.is_empty());
}

#[test]
fn rule_breaking_or_refused_runs_and_switched_off_retries_ask_right_away() {
    let (mut w, _f) = safe_world(true);
    retries(&mut w).failed.max = 0;
    let id = scan(&mut w).remove(0);
    let script = failed(&w);
    w.work(&script);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(hold(&w).releases_on(HoldRelease::NewHead));

    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = safe_result(&w, Some("low"), true);
    w.work_with_record(&script, |record| {
        for repository in &mut record.repositories {
            repository.read_only_violation = true;
        }
    });
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(hold(&w).releases.is_empty());
    move_source_head(&w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

/// Claude Code's adapter marks every run that ended in an error result; what ended it decides.
#[test]
fn a_failed_claude_run_is_retried_at_once_later_or_not_at_all() {
    use artidezi_contract::attempt::AttemptStatus;
    #[derive(Debug, PartialEq)]
    enum Next {
        AtOnce,
        Later,
        Person,
    }
    let api = |fields: &str| {
        format!(
            r#"{{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error",{fields}}}"#
        )
    };
    let overloaded = api(r#""api_error_status":529,"result":"API Error: 529 Overloaded.""#);
    let rejected = api(r#""api_error_status":401,"result":"Invalid API key""#);
    let spent = r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"terminal_reason":"budget_exhausted"}"#.to_owned();
    let unknown = api(r#""api_error_status":400,"result":"API Error: 400""#);
    let error = AttemptStatus::HarnessError;
    let done = "true".to_owned();
    for (event, status, before, retries, expected) in [
        (&overloaded, error, &done, 2, Next::AtOnce),
        (
            &overloaded,
            error,
            &"git commit -q --allow-empty -m Half".into(),
            2,
            Next::AtOnce,
        ),
        (&overloaded, error, &done, 0, Next::Later),
        (&overloaded, AttemptStatus::Timeout, &done, 2, Next::Later),
        // A run's result is not acted on when its harness failed afterwards, nor thrown away unseen.
        (
            &overloaded,
            error,
            &result_writer(
                r#"{"schema_version":1,"outcome":"failed","summary":"Broke.","repositories":[{"id":"role","changed":false}]}"#,
            ),
            2,
            Next::Later,
        ),
        (&unknown, error, &done, 2, Next::Later),
        (&rejected, error, &done, 2, Next::Person),
        (&spent, error, &done, 2, Next::Person),
    ] {
        let (mut w, _f) = safe_world(true);
        let config = &mut w.orchestrator.config;
        config.agent_profiles.get_mut("fake").unwrap().harness =
            artidezi_contract::job::Harness::Claude;
        let pool = config.worker_pools.get_mut("personal").unwrap();
        pool.harnesses = vec![artidezi_contract::job::Harness::Claude];
        pool.transient_failure_retries = retries;
        let endpoint = config.model_endpoints.get_mut("fake").unwrap();
        endpoint.provider = artidezi_config::model::ModelProvider::Anthropic;
        endpoint.auth_mode = "anthropic_api_key".into();
        let id = scan(&mut w).remove(0);
        let script = w.script(
            "claude.sh",
            &format!("{before}\nprintf '%s\\n' '{event}'\nexit 1"),
        );
        assert_eq!(
            w.work_with_record(&script, |record| {
                record.harness.name = "claude".into();
                record.status = status;
            }),
            AttemptState::Invalid
        );
        let next = match task_state(&w, &id) {
            TaskState::Working => Next::AtOnce,
            TaskState::WaitingToRetry => Next::Later,
            _ => Next::Person,
        };
        let attempt = w.orchestrator.db.list_attempts(&id).unwrap().remove(0);
        assert_eq!(
            next, expected,
            "{event} {before} {:?}",
            attempt.validation_problems
        );
        assert_eq!(
            w.orchestrator.db.list_attempts(&id).unwrap().len(),
            if next == Next::AtOnce { 2 } else { 1 }
        );
    }
}

#[test]
fn unreported_commits_that_cannot_be_inspected_ask_right_away() {
    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = w.script(
        "oversized.sh",
        &format!(
            "head -c 3000000 /dev/urandom | base64 > big.txt; git add -A; git commit -q -m Big; {}",
            result_writer(r#"{"schema_version":1,"outcome":"retry_later","summary":"Later.","retry_reason":"No image","retry_hint":"soon","repositories":[{"id":"role","changed":false}]}"#)
        ),
    );
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
    assert!(
        attempt
            .validation_problems
            .iter()
            .any(|problem| problem.contains("unreported commits could not be inspected")),
        "{:?}",
        attempt.validation_problems
    );
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

#[test]
fn unreported_commits_beyond_what_is_collected_or_inspected_ask_right_away() {
    for limit in ["message", "bundle", "partial", "missing"] {
        let (mut w, _f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        let message = match limit {
            "message" => "$(head -c 4000 /dev/zero | tr '\\0' x)",
            "partial" | "missing" => "Second",
            _ => {
                w.orchestrator.config.instance.limits.max_bundle_bytes = 1;
                "Change"
            }
        };
        let script = w.script(
            "unreported.sh",
            &format!(
                "git commit -q --allow-empty -m First; echo x > x.txt; git add -A; git commit -q -m \"{message}\"; {}",
                result_writer(r#"{"schema_version":1,"outcome":"failed","summary":"Broke.","repositories":[{"id":"role","changed":false}]}"#)
            ),
        );
        let state = w.work_with_record(&script, |record| {
            if limit == "partial" {
                // As a record that leaves out the first commit.
                for repository in &mut record.repositories {
                    if let Some(series) = &mut repository.series {
                        series.commits.remove(0);
                    }
                }
            }
            if limit == "missing" {
                for repository in &mut record.repositories {
                    repository.series = None;
                }
            }
        });
        assert_eq!(state, AttemptState::Invalid, "{limit}");
        let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        assert!(
            attempt
                .validation_problems
                .iter()
                .any(|problem| problem.contains("unreported commits could not be inspected")),
            "{limit}: {:?}",
            attempt.validation_problems
        );
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{limit}");
    }
}

#[test]
fn a_restart_keeps_a_lost_run_and_its_retry() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let pool = w
        .orchestrator
        .db
        .latest_attempt(&id)
        .unwrap()
        .unwrap()
        .worker_pool;
    w.orchestrator
        .lease(&pool, "worker", SignedDuration::from_secs(1))
        .unwrap()
        .unwrap();
    tick(&mut w, 700);
    assert_eq!(
        w.orchestrator
            .db
            .latest_attempt(&id)
            .unwrap()
            .unwrap()
            .state,
        AttemptState::Lost
    );
    tick(&mut w, 1400);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    reopen(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(tick(&mut w, 90_000).discovered.len(), 1);
    assert_eq!(source(&w).runs, 1);
}

#[test]
fn holds_of_an_older_release_recover_by_themselves() {
    // A question whose change was regenerated meanwhile.
    let (mut w, f, id) = asking();
    // Those releases recorded no digest of the published body.
    let mut receipt = w
        .orchestrator
        .db
        .list_events(&id)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "assessment.published")
        .unwrap()
        .payload;
    receipt.as_object_mut().unwrap().remove("body_sha256");
    w.orchestrator
        .db
        .record_event(
            Some(&id),
            None,
            "orchestrator",
            "assessment.published",
            receipt,
        )
        .unwrap();
    untype(&w, None, &id);
    move_source_head(&w, &f);
    reopen(&mut w, &f);
    assert_eq!(hold(&w).reason, HoldReason::Question);
    assert_eq!(settle(&mut w).len(), 1);
    assert_eq!(task_state(&w, &id), TaskState::Closed);

    // One that was merely rebased.
    let (mut w, f, id) = asking();
    untype(&w, None, &id);
    rebase_source(&w, &f);
    reopen(&mut w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(tick(&mut w, 3700).discovered.len(), 1);
    assert_eq!(source(&w).runs, 1);

    // A failed run.
    let (mut w, f) = safe_world(true);
    retries(&mut w).failed.max = 0;
    let id = scan(&mut w).remove(0);
    let script = failed(&w);
    w.work(&script);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    untype(&w, Some("agent reported failure; nothing published"), &id);
    retries(&mut w).failed.max = 1;
    reopen(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(tick(&mut w, 3700).discovered.len(), 1);

    // Delivery that an outage interrupted, and one that only a person can judge.
    for (reason, released) in [
        (
            "could not apply accepted attempt outcome: GitHub HTTP 503: temporary failure",
            true,
        ),
        (
            "publication retry withheld: existing assessment review differs; refusing a duplicate",
            false,
        ),
    ] {
        let (mut w, f) = safe_world(true);
        f.state.lock().unwrap().overrides.insert(
            "/repos/org/app/pulls/33/merge".into(),
            Reply::new(405, json!({"message":"fixture"})),
        );
        let id = scan(&mut w).remove(0);
        let script = safe_result(&w, Some("low"), true);
        w.work(&script);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        untype(&w, Some(reason), &id);
        move_source_head(&w, &f);
        reopen(&mut w, &f);
        assert_eq!(hold(&w).reason, HoldReason::Unclassified);
        assert_eq!(settle(&mut w).len(), usize::from(released), "{reason}");
    }
}

#[test]
fn a_refused_merge_waits_for_a_changed_head_or_target() {
    let (mut w, f) = safe_world(true);
    f.state.lock().unwrap().overrides.insert(
        "/repos/org/app/pulls/33/merge".into(),
        Reply::new(405, json!({"message":"Pull Request is not mergeable"})),
    );
    let id = scan(&mut w).remove(0);
    let script = safe_result(&w, Some("low"), true);
    w.work(&script);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(tick(&mut w, 90_000).discovered.is_empty());
    f.state
        .lock()
        .unwrap()
        .overrides
        .remove("/repos/org/app/pulls/33/merge");
    advance_target(&w);
    scan(&mut w);
    let queued = tick(&mut w, 95_000).discovered;
    assert_eq!(queued.len(), 1);
    w.work(&script);
    assert_eq!(task_state(&w, &queued[0]), TaskState::Done);
    assert!(f.state.lock().unwrap().merged);
}

#[test]
fn a_change_that_differs_before_its_retry_starts_uses_the_budget() {
    let (mut w, f) = safe_world(true);
    discovery(&mut w).settle_seconds = 60;
    discovery(&mut w).max_runs_per_pull_request = 1;
    scan(&mut w);
    let queued = tick(&mut w, 100).discovered;
    assert_eq!(queued.len(), 1);
    let script = failed(&w);
    w.work(&script);
    // Released, but held back by its checks.
    f.state.lock().unwrap().conclusion = "running".into();
    assert!(tick(&mut w, 3800).discovered.is_empty());
    assert_eq!(
        source(&w).reevaluations.pending,
        Some(EvaluationRetry::Failed)
    );
    move_source_head(&w, &f);
    f.state.lock().unwrap().conclusion = "success".into();
    assert!(tick(&mut w, 5000).discovered.is_empty());
    assert!(tick(&mut w, 6000).discovered.is_empty());
    assert_eq!(hold(&w).reason, HoldReason::Budget);
    assert_eq!(source(&w).runs, 1);
}

#[test]
fn a_question_survives_reopening_on_the_same_head_and_still_recovers() {
    let (mut w, f, id) = asking();
    f.state.lock().unwrap().open = false;
    scan(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    f.state.lock().unwrap().open = true;
    assert!(settle(&mut w).is_empty());
    assert_eq!(hold(&w).reason, HoldReason::Question);
    move_source_head(&w, &f);
    assert_eq!(settle(&mut w).len(), 1);
    assert_eq!(source(&w).runs, 2);
}

#[test]
fn a_rebase_during_the_run_beyond_the_budget_still_earns_its_own_retry() {
    let (mut w, f) = safe_world(true);
    discovery(&mut w).max_runs_per_pull_request = 1;
    let id = scan(&mut w).remove(0);
    rebase_source(&w, &f);
    let script = question(&w);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(settle(&mut w).is_empty());
    assert_eq!(hold(&w).retry, Some(EvaluationRetry::TargetChange));
    assert_eq!(tick(&mut w, 3700).discovered.len(), 1);
    assert_eq!(source(&w).runs, 1);
}

#[test]
fn a_retry_turned_off_before_it_started_does_not_use_the_budget() {
    let (mut w, f) = safe_world(true);
    scan(&mut w);
    let script = failed(&w);
    w.work(&script);
    f.state.lock().unwrap().conclusion = "running".into();
    assert!(tick(&mut w, 3700).discovered.is_empty());
    assert!(source(&w).reevaluations.pending.is_some());
    retries(&mut w).failed.max = 0;
    f.state.lock().unwrap().conclusion = "success".into();
    assert!(tick(&mut w, 5000).discovered.is_empty());
    assert_eq!(hold(&w).reason, HoldReason::Intake);
    assert_eq!(source(&w).runs, 1);
    move_source_head(&w, &f);
    assert_eq!(settle(&mut w).len(), 1);
    assert_eq!(source(&w).runs, 2);
}

#[test]
fn a_pause_of_an_older_release_survives_closing_upgrading_and_reopening() {
    let (mut w, f, id) = asking();
    w.orchestrator
        .db
        .record_event(Some(&id), None, "human", "discovery.paused", json!({}))
        .unwrap();
    untype(&w, None, &id);
    f.state.lock().unwrap().open = false;
    scan(&mut w);
    reopen(&mut w, &f);
    move_source_head(&w, &f);
    f.state.lock().unwrap().open = true;
    assert!(settle(&mut w).is_empty());
    assert_eq!(hold(&w).reason, HoldReason::OperatorPause);
}

#[test]
fn an_older_question_recovers_after_closing_upgrading_and_reopening_on_the_same_head() {
    for closed_by_hand in [false, true] {
        let (mut w, f, id) = asking();
        if closed_by_hand {
            w.orchestrator.close_task(&id, "human").unwrap();
        }
        untype(&w, None, &id);
        f.state.lock().unwrap().open = false;
        scan(&mut w);
        reopen(&mut w, &f);
        f.state.lock().unwrap().open = true;
        assert!(settle(&mut w).is_empty());
        move_source_head(&w, &f);
        let queued = settle(&mut w).len();
        assert_eq!(queued, usize::from(!closed_by_hand));
        if closed_by_hand {
            assert_eq!(hold(&w).reason, HoldReason::OperatorPause);
        }
    }
}

#[test]
fn an_approval_edited_while_its_merge_waits_keeps_a_moved_head_with_a_person() {
    let (mut w, f) = safe_world(true);
    f.state.lock().unwrap().overrides.insert(
        "/repos/org/app/pulls/33/merge".into(),
        Reply::new(503, json!({"message":"fixture"})),
    );
    let id = scan(&mut w).remove(0);
    let script = safe_result(&w, Some("low"), true);
    w.work(&script);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    {
        let mut state = f.state.lock().unwrap();
        let body = state.reviews[0]["body"].as_str().unwrap().to_owned();
        state.reviews[0]["body"] = json!(format!("{body}\nEdited."));
    }
    move_source_head(&w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

#[test]
fn a_temporary_blocker_is_evaluated_again_with_growing_waits_and_then_needs_a_person() {
    for shadow in [false, true] {
        let (mut w, f) = if shadow { world() } else { safe_world(true) };
        let mut id = scan(&mut w).remove(0);
        assert!(prompt(&w, &id).contains("\"retry_reason\": null"));
        assert!(prompt(&w, &id).contains("\"retry_hint\": null"));
        assert!(!prompt(&w, &id).contains("temporary blocker. Check"));
        assert!(prompt(&w, &id).contains("return `needs_human` (or `retry_later` for"));
        assert!(
            prompt(&w, &id).contains("broke in a way you cannot name as such a blocker. Every")
        );
        let script = blocked(&w);
        for (used, hours) in [1, 3, 9, 24].into_iter().enumerate() {
            assert_eq!(w.work(&script), AttemptState::Valid);
            assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
            let held = hold(&w);
            assert_eq!(held.reason, HoldReason::TemporaryBlocker);
            assert_eq!(held.retry, Some(EvaluationRetry::RetryLater));
            assert_eq!((held.retries, held.max_retries), (used as u32 + 1, 4));
            assert_eq!(held.message, "The image 1.2.3 is not published yet");
            let wait = (held.next_at.unwrap() - held.since).as_secs();
            assert!((wait - hours * 3600).abs() < 60, "{wait}");
            assert!(!source(&w).paused);
            assert!(f.state.lock().unwrap().reviews.is_empty());
            // Too early, even though the target moved.
            advance_target(&w);
            assert!(tick(&mut w, hours * 3600 - 600).discovered.is_empty());
            reopen(&mut w, &f);
            let queued = tick(&mut w, hours * 3600 + 120).discovered;
            assert_eq!(queued.len(), 1, "{shadow} {used}");
            assert_eq!(task_state(&w, &id), TaskState::Closed);
            assert_eq!(source(&w).runs, 1);
            assert_eq!(source(&w).reevaluations.retry_later, used as u32 + 1);
            id = queued[0].clone();
            let prompt = prompt(&w, &id);
            assert!(prompt.contains("was postponed because of a temporary blocker"));
            assert!(prompt.contains("The image 1.2.3 is not published yet"));
        }
        w.work(&script);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        let held = hold(&w);
        assert_eq!(held.reason, HoldReason::TemporaryBlocker);
        assert_eq!(
            held.message,
            "Still blocked after 4 retries: The image 1.2.3 is not published yet"
        );
        assert!(source(&w).paused);
        assert!(f.state.lock().unwrap().reviews.is_empty());
        assert!(tick(&mut w, 200_000).discovered.is_empty());
        // A change that differs starts over.
        move_source_head(&w, &f);
        let queued = settle(&mut w);
        assert_eq!(queued.len(), 1);
        assert_eq!(source(&w).runs, 2);
        assert_eq!(source(&w).reevaluations, Default::default());
        assert!(!prompt(&w, &queued[0]).contains("was postponed"));
    }
}

#[test]
fn a_cleared_blocker_is_delivered_and_people_end_the_wait() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = blocked(&w);
    w.work(&script);
    reopen(&mut w, &f);
    let queued = tick(&mut w, 3700).discovered;
    let safe = safe_result(&w, Some("low"), true);
    assert_eq!(w.work(&safe), AttemptState::Valid);
    assert_eq!(task_state(&w, &queued[0]), TaskState::Done);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert!(f.state.lock().unwrap().merged);

    // A rebase keeps the wait, an objection ends it.
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    let at = hold(&w).next_at;
    rebase_source(&w, &f);
    assert!(settle(&mut w).is_empty());
    assert_eq!(hold(&w).next_at, at);
    f.state.lock().unwrap().reviews.push(json!({"id": 900, "user": {"login": "maintainer", "type": "User"}, "commit_id": source(&w).observed_head, "body": "No.", "state": "CHANGES_REQUESTED"}));
    assert!(tick(&mut w, 3700).discovered.is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);

    // So do a pause and a closed source, and a different change does not wait.
    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    let tracked = source(&w);
    w.orchestrator
        .pause_pull_request(&tracked.id, "operator")
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(tick(&mut w, 90_000).discovered.is_empty());

    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    f.state.lock().unwrap().open = false;
    scan(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(source(&w).hold, None);

    let (mut w, f) = safe_world(true);
    scan(&mut w);
    w.work(&script);
    move_source_head(&w, &f);
    assert_eq!(settle(&mut w).len(), 1);
    assert_eq!(source(&w).runs, 2);
}

#[test]
fn a_slow_blocker_is_checked_weekly_and_updates_do_not_extend_the_wait() {
    const WEEK: i64 = 7 * 86_400;
    let (mut w, f) = safe_world(true);
    discovery(&mut w).max_runs_per_pull_request = 2;
    retries(&mut w).retry_later.slow.max = 3;
    let mut id = scan(&mut w).remove(0);
    let script = slowly_blocked(&w);
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    let held = hold(&w);
    assert_eq!(held.reason, HoldReason::TemporaryBlocker);
    assert_eq!(held.retry, Some(EvaluationRetry::RetryLaterSlow));
    assert_eq!((held.retries, held.max_retries), (1, 3));
    assert!(((held.next_at.unwrap() - held.since).as_secs() - WEEK).abs() < 60);
    assert!(!source(&w).paused);
    advance_target(&w);
    assert!(tick(&mut w, WEEK - 600).discovered.is_empty());
    reopen(&mut w, &f);
    id = tick(&mut w, WEEK + 120).discovered.remove(0);
    assert_eq!(source(&w).runs, 1);
    assert_eq!(source(&w).reevaluations.retry_later_slow, 1);
    assert!(prompt(&w, &id).contains("The merged fix is not released yet"));

    // A change that differs is evaluated at once within the run budget, without a new wait.
    w.work(&script);
    move_source_head_to(&w, &f, "2.0.0");
    id = settle(&mut w).remove(0);
    assert_eq!(source(&w).runs, 2);
    assert_eq!(source(&w).reevaluations.retry_later_slow, 1);
    w.work(&script);
    let held = hold(&w);
    assert_eq!((held.retries, held.max_retries), (2, 3));

    // Without budget it waits with the blocker instead of asking a person, and is looked at once.
    move_source_head_to(&w, &f, "3.0.0");
    assert!(settle(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    let held = hold(&w);
    assert_eq!(
        held.rebased_head.as_deref(),
        Some(source(&w).observed_head.as_str())
    );
    assert_eq!(held.blocked_until, None);
    reopen(&mut w, &f);
    let queued = tick(&mut w, WEEK + 120).discovered;
    assert_eq!(queued.len(), 1);
    id = queued[0].clone();
    assert!(prompt(&w, &id).contains("The merged fix is not released yet"));
    assert_eq!(source(&w).runs, 2);
    assert_eq!(source(&w).reevaluations.retry_later_slow, 2);

    // Also when it moves only right before the check, and the checks run out all the same.
    w.work(&script);
    assert_eq!(hold(&w).retries, 3);
    move_source_head_to(&w, &f, "4.0.0");
    reopen(&mut w, &f);
    let mut queued = tick(&mut w, WEEK + 120).discovered;
    queued.extend(tick(&mut w, WEEK + 240).discovered);
    assert_eq!(queued.len(), 1);
    id = queued.remove(0);
    assert_eq!(source(&w).reevaluations.retry_later_slow, 3);
    w.work(&script);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(
        hold(&w).message,
        "Still blocked after 3 retries: The merged fix is not released yet"
    );
    assert!(f.state.lock().unwrap().reviews.is_empty());
    let tracked = source(&w);
    w.orchestrator
        .resume_pull_request(&tracked.id, "operator")
        .unwrap();
    assert_eq!(source(&w).reevaluations, Default::default());
}

#[test]
fn a_blocker_can_turn_out_slow_and_slow_checks_can_be_turned_off() {
    let (mut w, f) = safe_world(true);
    scan(&mut w);
    let script = blocked(&w);
    w.work(&script);
    reopen(&mut w, &f);
    assert_eq!(tick(&mut w, 3700).discovered.len(), 1);
    let script = slowly_blocked(&w);
    w.work(&script);
    let held = hold(&w);
    assert_eq!(held.retry, Some(EvaluationRetry::RetryLaterSlow));
    assert_eq!((held.retries, held.max_retries), (1, 6));
    assert_eq!(source(&w).reevaluations.retry_later, 1);

    for all in [false, true] {
        let (mut w, _f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        if all {
            retries(&mut w).retry_later.max = 0;
        } else {
            retries(&mut w).retry_later.slow.max = 0;
        }
        let script = slowly_blocked(&w);
        assert_eq!(w.work(&script), AttemptState::Valid);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        assert_eq!(hold(&w).message, "The merged fix is not released yet");
    }
}

#[test]
fn switched_off_blocker_retries_are_not_offered_and_stop_waiting() {
    let (mut w, _f) = safe_world(true);
    retries(&mut w).retry_later.max = 0;
    let id = scan(&mut w).remove(0);
    assert!(!prompt(&w, &id).contains("retry_later"));
    assert!(
        prompt(&w, &id).contains("Use `failed` when the environment prevented the work. Every")
    );
    assert!(!prompt(&w, &id).contains("retry_reason"));
    let script = blocked(&w);
    assert_eq!(w.work(&script), AttemptState::Invalid);

    // Turned off during the run, and during the wait.
    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    retries(&mut w).retry_later.max = 0;
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(hold(&w).message, "The image 1.2.3 is not published yet");

    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    w.work(&script);
    retries(&mut w).retry_later.max = 0;
    assert!(tick(&mut w, 3700).discovered.is_empty());
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

#[test]
fn a_restart_before_a_reported_blocker_was_recorded_still_waits() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = blocked(&w);
    w.work(&script);
    // As a crash between accepting the result and recording its hold leaves it.
    w.orchestrator
        .db
        .set_task_state(&id, TaskState::Working, "test")
        .unwrap();
    let mut tracked = source(&w);
    tracked.release_hold();
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    discovery(&mut w).retries.delivery.max = 0;
    reopen(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(hold(&w).reason, HoldReason::TemporaryBlocker);
    assert_eq!(hold(&w).retries, 1);
    assert_eq!(tick(&mut w, 8000).discovered.len(), 1);
}

#[test]
fn a_restart_goes_on_without_a_result_that_is_gone() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = blocked(&w);
    w.work(&script);
    w.orchestrator
        .db
        .set_task_state(&id, TaskState::Working, "test")
        .unwrap();
    let mut tracked = source(&w);
    tracked.release_hold();
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
    let result = attempt.result.unwrap();
    std::fs::remove_file(w.orchestrator.artifacts.path(&result).unwrap()).unwrap();
    reopen(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(hold(&w).message.contains("cannot be read"));
}

/// Those releases planned the exact body of an approval, but not of a comment.
#[test]
fn a_comment_of_an_older_release_that_was_edited_stays_with_a_person() {
    for case in ["untouched", "inserted", "paragraph", "removed"] {
        let (mut w, f, id) = asking();
        let mut receipt = event(&w, &id, "assessment.published");
        receipt.as_object_mut().unwrap().remove("body_sha256");
        // Only the older form of it is left.
        let mut state = f.state.lock().unwrap();
        let mut review = state.reviews[0].clone();
        review["id"] = json!(901);
        receipt["review_id"] = json!(901);
        if case == "inserted" {
            let body = review["body"].as_str().unwrap();
            let marker = body.find("<!-- artidezi:assessment:").unwrap();
            review["body"] = json!(format!(
                "{}Do not approve this.\n\n{}",
                &body[..marker],
                &body[marker..]
            ));
        }
        if case == "paragraph" {
            // Only an approval can say this.
            let body = review["body"].as_str().unwrap();
            let end = body.find("\n\nAssessed source").unwrap();
            let text = body[..end].trim_end();
            review["body"] = json!(format!(
                "{text}\n\n**No forge checks were reported for this commit.** This handler accepts that, so the approval rests on ArtiDezi's assessment alone.{}",
                &body[text.len()..]
            ));
        }
        if case != "removed" {
            state.reviews.push(review);
        }
        drop(state);
        let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        w.orchestrator
            .db
            .record_event(
                Some(&id),
                Some(&attempt.id),
                "orchestrator",
                "assessment.published",
                receipt,
            )
            .unwrap();
        untype(&w, None, &id);
        move_source_head(&w, &f);
        reopen(&mut w, &f);
        assert_eq!(hold(&w).reason, HoldReason::Question);
        assert_eq!(
            settle(&mut w).len(),
            usize::from(case == "untouched"),
            "{case}"
        );
    }
}

/// Those releases posted comments without a plan: a restart could leave one without any record.
#[test]
fn an_unrecorded_comment_of_an_older_release_that_was_edited_stays_with_a_person() {
    for moved in [false, true] {
        for edited in [false, true] {
            let (mut w, f) = safe_world(true);
            let id = scan(&mut w).remove(0);
            // As a crash between accepting the result and any record of its delivery leaves it.
            f.state.lock().unwrap().overrides.insert(
                "/repos/org/app/pulls/33".into(),
                Reply::new(503, json!({"message":"fixture"})),
            );
            let script = question(&w);
            assert_eq!(w.work(&script), AttemptState::Valid);
            f.state.lock().unwrap().overrides.clear();
            assert!(
                w.orchestrator
                    .db
                    .list_events(&id)
                    .unwrap()
                    .iter()
                    .all(|event| !event.kind.starts_with("assessment."))
            );
            w.orchestrator
                .db
                .set_task_state(&id, TaskState::Working, "test")
                .unwrap();
            let mut tracked = source(&w);
            tracked.release_hold();
            w.orchestrator
                .db
                .save_pull_request_source(&tracked)
                .unwrap();
            reopen(&mut w, &f);
            assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
            if edited {
                f.state.lock().unwrap().reviews.push(json!({"id": 901, "user": {"login": "artidezi-fixture[bot]", "type": "Bot"}, "commit_id": source(&w).observed_head, "body": "Do not approve this.", "state": "COMMENTED"}));
            }
            if moved {
                move_source_head(&w, &f);
            }
            // The delivery retry, then whatever its end leads to.
            let mut discovered = 0;
            for seconds in [700, 1400, 4000] {
                scan(&mut w);
                discovered += tick(&mut w, seconds).discovered.len();
            }
            let case = format!("moved {moved}, edited {edited}");
            assert_eq!(
                source(&w).runs,
                if moved && !edited { 2 } else { 1 },
                "{case}"
            );
            let reviews = f.state.lock().unwrap().reviews.len();
            if moved && !edited {
                assert!(discovered <= 1, "{case}");
            } else {
                assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{case}");
                assert_eq!(reviews, 1, "{case}");
                assert_eq!(hold(&w).reason == HoldReason::Question, !edited, "{case}");
            }
        }
    }
}

/// Those releases planned the exact body of an approval, but its receipt had no digest.
#[test]
fn an_approval_of_an_older_release_that_was_edited_stays_with_a_person() {
    for edited in [true, false] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().lose_review_response = true;
        let script = safe_result(&w, Some("low"), true);
        w.work(&script);
        let review = f.state.lock().unwrap().reviews[0].clone();
        let head = source(&w).observed_head;
        w.orchestrator
            .db
            .record_event(
                Some(&id),
                None,
                "orchestrator",
                "assessment.published",
                json!({"review_id": review["id"], "head": head, "outcome": "approve_untouched", "state": "approved"}),
            )
            .unwrap();
        if edited {
            let body = review["body"].as_str().unwrap();
            let marker = body.find("<!-- artidezi:assessment:").unwrap();
            f.state.lock().unwrap().reviews[0]["body"] = json!(format!(
                "{}Not yet.\n\n{}",
                &body[..marker],
                &body[marker..]
            ));
        }
        w.orchestrator
            .db
            .set_task_state(&id, TaskState::NeedsHuman, "test")
            .unwrap();
        untype(
            &w,
            Some("could not apply accepted attempt outcome: GitHub HTTP 503: temporary failure"),
            &id,
        );
        move_source_head(&w, &f);
        reopen(&mut w, &f);
        assert_eq!(settle(&mut w).len(), usize::from(!edited), "{edited}");
        assert!(!f.state.lock().unwrap().merged);
    }
}

#[test]
fn an_older_run_that_moved_its_commits_stays_with_a_person() {
    for committed in [true, false] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        let script = if committed {
            w.script(
                "commit.sh",
                "echo x >> CHANGES.md; git add -A; git commit -q -m Change",
            )
        } else {
            failed(&w)
        };
        w.work_with_record(&script, |record| record.result = None);
        let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        assert_eq!(attempt.state, AttemptState::Invalid);
        assert_eq!(
            attempt
                .validation_problems
                .iter()
                .any(|problem| problem.contains("reported unchanged but HEAD moved")),
            committed,
            "{:?}",
            attempt.validation_problems
        );
        w.orchestrator
            .db
            .set_task_state(&id, TaskState::NeedsHuman, "test")
            .unwrap();
        untype(&w, None, &id);
        reopen(&mut w, &f);
        assert_eq!(
            task_state(&w, &id) == TaskState::WaitingToRetry,
            !committed,
            "{committed}"
        );
        assert_eq!(hold(&w).releases.is_empty(), committed);
    }
}
