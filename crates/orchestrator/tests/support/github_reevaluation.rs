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
    w.script("blocked.sh", &result_writer(r#"{"schema_version":1,"outcome":"retry_later","summary":"Not decidable yet.","retry_reason":"The image 1.2.3\nis not published yet","repositories":[{"id":"role","changed":false}]}"#))
}
fn prompt(w: &World, id: &str) -> String {
    let attempt = w.orchestrator.db.latest_attempt(id).unwrap().unwrap();
    let job = w.orchestrator.load_job(&attempt).unwrap();
    assert_eq!(job.schema_version, 3);
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

#[test]
fn unreported_commits_that_cannot_be_inspected_ask_right_away() {
    let (mut w, _f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = w.script(
        "oversized.sh",
        &format!(
            "head -c 3000000 /dev/urandom | base64 > big.txt; git add -A; git commit -q -m Big; {}",
            result_writer(r#"{"schema_version":1,"outcome":"retry_later","summary":"Later.","retry_reason":"No image","repositories":[{"id":"role","changed":false}]}"#)
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
