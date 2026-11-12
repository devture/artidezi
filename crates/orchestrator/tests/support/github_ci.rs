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

fn held_source(w: &World) -> TrackedPullRequest {
    w.orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0)
}

fn writes(f: &Fixture) -> usize {
    f.state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.method != "GET" && !r.path.starts_with("/app/"))
        .count()
}

/// Failed checks before approval, while waiting, or after approval (the merge is refused) all
/// hold delivery; a re-run that turns the same head green continues it without another model run.
#[test]
fn failed_checks_hold_until_the_same_head_turns_green() {
    for stage in ["intake", "waiting", "after_approval"] {
        let (mut w, f, id) = match stage {
            "waiting" => {
                let (w, f, id) = wait(true, false);
                f.state.lock().unwrap().conclusion = "failure".into();
                (w, f, id)
            }
            _ => {
                let (mut w, f) = safe_world(true);
                let id = scan(&mut w).remove(0);
                if stage == "intake" {
                    f.state.lock().unwrap().conclusion = "failure".into();
                } else {
                    f.state.lock().unwrap().fail_on_approval = true;
                }
                assert_eq!(
                    w.work(&safe_result(&w, Some("low"), true)),
                    AttemptState::Valid
                );
                (w, f, id)
            }
        };
        let now = Utc::now() + Duration::seconds(601);
        if stage == "waiting" {
            w.orchestrator.tick_at(now).unwrap();
        }
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{stage}");
        let source = held_source(&w);
        assert!(source.paused, "{stage}");
        assert!(
            source.status.starts_with("Current forge checks failed"),
            "{stage}"
        );
        assert!(!f.state.lock().unwrap().merged, "{stage}");
        let approvals = usize::from(stage == "after_approval");
        assert_eq!(f.state.lock().unwrap().reviews.len(), approvals, "{stage}");

        // Still red after settling: the hold stays and nothing is written.
        reopen(&mut w, &f);
        let before = writes(&f);
        w.orchestrator
            .tick_at(now + Duration::seconds(601))
            .unwrap();
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{stage}");
        assert_eq!(writes(&f), before, "{stage}");

        // A re-run passes on the same head; the recheck follows the delivery cadence.
        f.state.lock().unwrap().conclusion = "success".into();
        let report = w
            .orchestrator
            .tick_at(now + Duration::seconds(1202))
            .unwrap();
        assert!(report.errors.is_empty(), "{stage}: {:?}", report.errors);
        assert_eq!(task_state(&w, &id), TaskState::Done, "{stage}");
        assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
        assert_eq!(held_source(&w).runs, 1, "{stage}");
        let s = f.state.lock().unwrap();
        assert!(s.merged, "{stage}");
        assert_eq!(
            s.reviews.len(),
            1,
            "{stage}: the approval is not duplicated"
        );
        assert_eq!(s.requests.iter().filter(|r| r.method == "PUT").count(), 1);
    }
}

#[test]
fn failed_checks_hold_reassesses_a_new_head_within_the_budget() {
    for (max_runs, equivalent) in [(2, false), (1, false), (2, true), (1, true)] {
        let (mut w, f) = safe_world(true);
        w.orchestrator
            .config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .max_runs_per_pull_request = max_runs;
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().fail_on_approval = true;
        w.work(&safe_result(&w, Some("low"), true));
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        let new_head = if equivalent {
            super::supersession::rebase_source(&w, &f).1
        } else {
            move_source_head(&w, &f)
        };
        f.state.lock().unwrap().fail_on_approval = false;
        f.state.lock().unwrap().conclusion = "success".into();
        // The settled new head is retired and queued for a fresh assessment in one pass.
        let report = w
            .orchestrator
            .tick_at(Utc::now() + Duration::seconds(601))
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(task_state(&w, &id), TaskState::Closed, "{max_runs}");
        let source = held_source(&w);
        assert_eq!(source.observed_head, new_head.0);
        assert!(!f.state.lock().unwrap().merged);
        if max_runs == 1 {
            assert!(source.paused);
            assert_eq!(source.runs, 1, "superseding never refunds the budget");
            assert!(source.status.contains("budget"), "{}", source.status);
            assert!(report.discovered.is_empty());
            continue;
        }
        assert!(!source.paused);
        assert_eq!(source.runs, 2, "the new head uses the existing budget");
        assert_eq!(report.discovered.len(), 1);
        let next = report.discovered[0].clone();
        assert_eq!(
            artidezi_orchestrator::discovery::source_of(
                &w.orchestrator.db.get_task(&next).unwrap()
            )
            .unwrap()
            .unwrap()
            .input
            .commit,
            new_head
        );
        assert_eq!(
            w.work(&safe_result(&w, Some("low"), true)),
            AttemptState::Valid
        );
        assert_eq!(task_state(&w, &next), TaskState::Done);
        let s = f.state.lock().unwrap();
        assert!(s.merged);
        assert_eq!(s.reviews.last().unwrap()["commit_id"], new_head.0);
    }
}

#[test]
fn failed_checks_recovery_respects_explicit_pauses_and_new_objections() {
    for later in ["pause", "objection", "objection_then_new_head"] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().conclusion = "failure".into();
        w.work(&safe_result(&w, Some("low"), true));
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        let now = Utc::now() + Duration::seconds(601);
        match later {
            "pause" => {
                let tracking = held_source(&w).id;
                w.orchestrator
                    .pause_pull_request(&tracking, "human")
                    .unwrap();
            }
            "objection" | "objection_then_new_head" => {
                let mut s = f.state.lock().unwrap();
                let head = s.head.clone();
                s.reviews.push(json!({"id":50,"user":{"login":"maintainer","type":"User"},"commit_id":head,"body":"Needs a fix","state":"CHANGES_REQUESTED"}));
            }
            _ => unreachable!(),
        }
        if later == "objection_then_new_head" {
            move_source_head(&w, &f);
        }
        f.state.lock().unwrap().conclusion = "success".into();
        for step in 1..=3 {
            reopen(&mut w, &f);
            w.orchestrator
                .tick_at(now + Duration::seconds(601 * step))
                .unwrap();
        }
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{later}");
        assert!(held_source(&w).paused, "{later}");
        assert_eq!(held_source(&w).runs, 1, "{later}");
        let s = f.state.lock().unwrap();
        assert!(!s.merged, "{later}");
        assert!(
            s.requests
                .iter()
                .all(|r| r.method == "GET" || r.path.starts_with("/app/")),
            "{later}"
        );
    }
}

/// A lost approval response is an unconfirmed write. Failed checks on a later retry must not turn
/// it into a self-releasing hold, even when the source moves to a new head.
#[test]
fn failed_checks_after_an_unconfirmed_approval_stay_with_a_person() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().lose_review_response = true;
    w.work(&safe_result(&w, Some("low"), true));
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    f.state.lock().unwrap().conclusion = "failure".into();
    w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let source = held_source(&w);
    assert!(source.paused);
    assert!(source.status.contains("unconfirmed"), "{}", source.status);

    move_source_head(&w, &f);
    f.state.lock().unwrap().conclusion = "success".into();
    let now = Utc::now();
    for step in 1..=3 {
        reopen(&mut w, &f);
        w.orchestrator
            .tick_at(now + Duration::seconds(601 * step))
            .unwrap();
    }
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(held_source(&w).paused);
    assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
    let s = f.state.lock().unwrap();
    assert!(!s.merged);
    assert_eq!(s.reviews.len(), 1);
}

/// One task can fail checks, recover and fail again; each hold is rechecked and the approval is
/// posted once.
#[test]
fn repeated_failed_checks_cycles_on_one_task_deliver_once() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().conclusion = "failure".into();
    w.work(&safe_result(&w, Some("low"), true));
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let now = Utc::now();
    // Green again: the retry posts the approval, then checks fail before the merge.
    {
        let mut s = f.state.lock().unwrap();
        s.conclusion = "success".into();
        s.fail_on_approval = true;
    }
    w.orchestrator
        .tick_at(now + Duration::seconds(601))
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(
        held_source(&w)
            .status
            .starts_with("Current forge checks failed")
    );
    {
        let s = f.state.lock().unwrap();
        assert_eq!(s.reviews.len(), 1);
        assert!(!s.merged);
    }
    // Still red on the next check, then green.
    reopen(&mut w, &f);
    w.orchestrator
        .tick_at(now + Duration::seconds(1202))
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    {
        let mut s = f.state.lock().unwrap();
        s.fail_on_approval = false;
        s.conclusion = "success".into();
    }
    reopen(&mut w, &f);
    let report = w
        .orchestrator
        .tick_at(now + Duration::seconds(1803))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(task_state(&w, &id), TaskState::Done);
    assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
    assert_eq!(held_source(&w).runs, 1);
    let s = f.state.lock().unwrap();
    assert!(s.merged);
    assert_eq!(s.reviews.len(), 1);
    assert_eq!(s.requests.iter().filter(|r| r.method == "PUT").count(), 1);
}

/// A planned merge without a receipt is unconfirmed. Failed checks on retry keep an ordinary hold.
#[test]
fn failed_checks_after_an_unconfirmed_merge_stay_with_a_person() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let head = f.state.lock().unwrap().head.clone();
    f.state.lock().unwrap().move_head_on_merge = true;
    w.work(&safe_result(&w, Some("low"), true));
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(
        w.orchestrator
            .db
            .list_events(&id)
            .unwrap()
            .iter()
            .any(|e| e.kind == "source.merge_planned")
    );
    {
        let mut s = f.state.lock().unwrap();
        s.head = head;
        s.conclusion = "failure".into();
    }
    w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(held_source(&w).status.contains("unconfirmed"));
    move_source_head(&w, &f);
    f.state.lock().unwrap().conclusion = "success".into();
    let now = Utc::now();
    for step in 1..=3 {
        reopen(&mut w, &f);
        w.orchestrator
            .tick_at(now + Duration::seconds(601 * step))
            .unwrap();
    }
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(held_source(&w).paused);
    assert!(!f.state.lock().unwrap().merged);
}
