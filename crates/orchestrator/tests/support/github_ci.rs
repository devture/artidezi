// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use super::*;
use chrono::{Duration, Utc};

#[test]
fn checks_restarted_after_approval_do_not_duplicate_reviews_or_lose_external_merges() {
    for external_merge in [false, true] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().pending_on_approval = true;
        w.work(&safe_result(&w, Some("low"), true));
        assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
        assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
        assert!(!f.state.lock().unwrap().merged);
        {
            let mut s = f.state.lock().unwrap();
            s.conclusion = "success".into();
            s.open = !external_merge;
            s.merged = external_merge;
        }
        // A discovery scan before the delivery deadline must not discard reconciliation.
        assert!(scan(&mut w).is_empty());
        if external_merge {
            assert!(w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].closed);
            f.state.lock().unwrap().requests.clear();
            assert!(scan(&mut w).is_empty());
            assert!(
                f.state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .any(|request| request.path == "/repos/org/app/pulls/33"),
                "a closed source with a waiting task still needs the existing reconciliation scan"
            );
        }
        reopen(&mut w, &f);
        w.orchestrator
            .tick_at(Utc::now() + Duration::seconds(601))
            .unwrap();
        assert_eq!(task_state(&w, &id), TaskState::Done);
        if external_merge {
            f.state.lock().unwrap().requests.clear();
            assert!(scan(&mut w).is_empty());
            assert!(
                !f.state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .any(|request| request.path == "/repos/org/app/pulls/33"),
                "a settled closed source no longer needs an individual fetch"
            );
        }
        let s = f.state.lock().unwrap();
        assert_eq!(s.reviews.len(), 1);
        assert_eq!(
            s.requests.iter().filter(|r| r.method == "PUT").count(),
            usize::from(!external_merge)
        );
    }
}

fn wait(merge: bool, intake_checks: bool) -> (World, Fixture, String) {
    let (mut w, f) = safe_world(merge);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .require_successful_checks = intake_checks;
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().conclusion = "running".into();
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    no_publication(&f);
    (w, f, id)
}

#[test]
fn pending_ci_resumes_approval_or_merge_once_without_another_worker_across_restarts() {
    for (merge, intake_checks) in [(false, false), (true, false), (true, true)] {
        let (mut w, f, id) = wait(merge, intake_checks);
        let now = Utc::now();
        let original = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        assert!(scan(&mut w).is_empty());
        assert!(
            w.orchestrator
                .lease("personal", "unused", Duration::minutes(1))
                .unwrap()
                .is_none()
        );
        assert!(!w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused);
        reopen(&mut w, &f);
        assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
        let count = f.state.lock().unwrap().requests.len();
        w.orchestrator.tick_at(now).unwrap();
        assert_eq!(f.state.lock().unwrap().requests.len(), count, "not due yet");
        let report = w
            .orchestrator
            .tick_at(now + Duration::seconds(601))
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
        assert!(report.discovered.is_empty());
        no_publication(&f);
        f.state.lock().unwrap().conclusion = "success".into();
        reopen(&mut w, &f);
        let count = f.state.lock().unwrap().requests.len();
        w.orchestrator
            .tick_at(now + Duration::seconds(602))
            .unwrap();
        assert_eq!(
            f.state.lock().unwrap().requests.len(),
            count,
            "poll deadline survives restart"
        );
        let report = w
            .orchestrator
            .tick_at(now + Duration::seconds(1202))
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(task_state(&w, &id), TaskState::Done);
        assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
        let current = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        assert_eq!(
            (current.id, current.job, current.result),
            (original.id, original.job, original.result)
        );
        reopen(&mut w, &f);
        w.orchestrator
            .tick_at(now + Duration::seconds(1803))
            .unwrap();
        let s = f.state.lock().unwrap();
        assert_eq!(s.reviews.len(), 1);
        assert_eq!(s.reviews[0]["state"], "APPROVED");
        assert_eq!(s.merged, merge);
        assert_eq!(
            s.requests
                .iter()
                .filter(|r| r.method == "POST" && r.path.ends_with("/reviews"))
                .count(),
            1
        );
        assert_eq!(
            s.requests.iter().filter(|r| r.method == "PUT").count(),
            usize::from(merge)
        );
        assert!(s.proposals.is_empty());
    }
}

#[test]
fn waiting_never_bypasses_failures_changed_inputs_policy_or_human_intervention() {
    for reason in [
        "failed",
        "target",
        "context",
        "closed",
        "merged",
        "draft",
        "objection",
        "policy",
        "project_removed",
        "pause",
        "close_task",
    ] {
        let (mut w, f, id) = wait(true, false);
        f.state.lock().unwrap().conclusion = "success".into();
        match reason {
            "failed" => f.state.lock().unwrap().conclusion = "failure".into(),
            "closed" => f.state.lock().unwrap().open = false,
            "merged" => {
                let mut s = f.state.lock().unwrap();
                s.open = false;
                s.merged = true;
            }
            "draft" => f.state.lock().unwrap().draft = true,
            "target" | "context" => {
                let repo = w
                    .forge
                    .ensure_repository(if reason == "target" {
                        "org/app"
                    } else {
                        "org/playbook"
                    })
                    .unwrap();
                let base = repo.refname_to_id("refs/heads/main").unwrap();
                seed(
                    &repo,
                    Some(base),
                    "README.md",
                    "Changed code",
                    "Changed base",
                );
            }
            "objection" => {
                let mut s = f.state.lock().unwrap();
                let head = s.head.clone();
                s.reviews.push(json!({"id":50,"user":{"login":"maintainer","type":"User"},"commit_id":head,"body":"Needs a fix","state":"CHANGES_REQUESTED"}));
            }
            "policy" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .effects
                    .merge_renovate_merge_request_on_safe_verdict = false
            }
            "project_removed" => {
                w.orchestrator.config.projects.remove("roles");
            }
            "pause" => {
                let source = artidezi_orchestrator::discovery::source_of(
                    &w.orchestrator.db.get_task(&id).unwrap(),
                )
                .unwrap()
                .unwrap();
                w.orchestrator
                    .pause_pull_request(&source.tracking_id, "human")
                    .unwrap();
            }
            "close_task" => {
                w.orchestrator.close_task(&id, "human").unwrap();
            }
            _ => unreachable!(),
        }
        let now = Utc::now() + Duration::seconds(601);
        w.orchestrator.tick_at(now).unwrap();
        assert_eq!(
            task_state(&w, &id),
            if matches!(reason, "close_task" | "project_removed") {
                TaskState::Closed
            } else if matches!(reason, "target" | "context") {
                TaskState::Done
            } else {
                TaskState::NeedsHuman
            },
            "{reason}"
        );
        f.state.lock().unwrap().conclusion = "success".into();
        // Resolving the external condition does not silently release a human hold.
        w.orchestrator
            .tick_at(now + Duration::seconds(601))
            .unwrap();
        assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
        let s = f.state.lock().unwrap();
        assert!(
            s.requests
                .iter()
                .all(|r| matches!(reason, "target" | "context")
                    || r.method == "GET"
                    || r.path.starts_with("/app/")),
            "{reason}"
        );
    }
}

#[test]
fn waiting_respects_backoff_and_lost_write_responses_still_need_explicit_recovery() {
    let (mut w, f, id) = wait(true, false);
    w.orchestrator
        .db
        .defer_forge("github", Utc::now() + Duration::hours(1))
        .unwrap();
    reopen(&mut w, &f);
    let count = f.state.lock().unwrap().requests.len();
    w.orchestrator
        .tick_at(Utc::now() + Duration::seconds(601))
        .unwrap();
    assert_eq!(f.state.lock().unwrap().requests.len(), count);
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);

    let (mut w, f, id) = wait(true, false);
    {
        let mut s = f.state.lock().unwrap();
        s.conclusion = "success".into();
        s.lose_review_response = true;
    }
    let now = Utc::now() + Duration::seconds(601);
    w.orchestrator.tick_at(now).unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    reopen(&mut w, &f);
    w.orchestrator
        .tick_at(now + Duration::seconds(601))
        .unwrap();
    assert!(!f.state.lock().unwrap().merged);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
    w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task_state(&w, &id), TaskState::Done);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
}

#[test]
fn ordinary_publication_holds_and_nonqualifying_results_are_not_automatic_waits() {
    for reason in ["objection", "comment", "proposal"] {
        let (mut w, f) = if reason == "comment" {
            publishing_world()
        } else {
            safe_world(true)
        };
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().conclusion = "running".into();
        if reason == "objection" {
            let mut s = f.state.lock().unwrap();
            let head = s.head.clone();
            s.reviews.push(json!({"id":50,"user":{"login":"maintainer","type":"User"},"commit_id":head,"body":"Needs a fix","state":"CHANGES_REQUESTED"}));
        }
        let script = if reason == "proposal" {
            proposal(&w)
        } else {
            safe_result(&w, Some("low"), true)
        };
        w.work(&script);
        assert_eq!(
            task_state(&w, &id),
            if reason == "proposal" {
                TaskState::AwaitingReview
            } else {
                TaskState::NeedsHuman
            },
            "{reason}"
        );
        f.state.lock().unwrap().conclusion = "success".into();
        w.orchestrator
            .tick_at(Utc::now() + Duration::seconds(601))
            .unwrap();
        assert!(!f.state.lock().unwrap().merged, "{reason}");
        assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
    }
}
