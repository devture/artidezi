// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Delivery retries: what is repeated, what is looked up first, and what stays with a person.

use super::*;

const MERGE: &str = "/repos/org/app/pulls/33/merge";
const REVIEWS: &str = "/repos/org/app/pulls/33/reviews";

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
fn respond(f: &Fixture, path: &str, status: u16) {
    f.state.lock().unwrap().overrides.insert(
        path.into(),
        Reply::new(status, json!({"message":"fixture"})),
    );
}
fn restore(f: &Fixture, path: &str) {
    f.state.lock().unwrap().overrides.remove(path);
}
fn sent(f: &Fixture, method: &str, path: &str) -> usize {
    f.state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.method == method && r.path == path)
        .count()
}
/// A pass after any scheduled retry is due.
fn later(w: &mut World, f: &Fixture) {
    reopen(w, f);
    w.orchestrator
        .tick_at(Timestamp::now() + SignedDuration::from_secs(90_000))
        .unwrap();
}
fn safe_run() -> (World, Fixture, String) {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    (w, f, id)
}
fn finish(w: &mut World) {
    let script = safe_result(w, Some("low"), true);
    assert_eq!(w.work(&script), AttemptState::Valid);
}
fn limits(w: &mut World) -> &mut artidezi_config::model::DeliveryRetries {
    &mut w
        .orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .retries
        .delivery
}

#[test]
fn a_temporary_merge_failure_is_retried_with_backoff_without_another_worker_or_approval() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    let before = Timestamp::now();
    finish(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    let first = hold(&w);
    assert_eq!(first.reason, HoldReason::Delivery);
    assert_eq!((first.retries, first.max_retries), (1, 8));
    assert!(first.message.contains("503") && first.message.ends_with("while merging"));
    let wait = first.next_at.unwrap().duration_since(before).as_secs();
    assert!((115..=130).contains(&wait), "{wait}");
    assert!(!source(&w).paused);

    // Discovery and early passes leave it alone.
    assert!(scan(&mut w).is_empty());
    reopen(&mut w, &f);
    w.orchestrator.tick().unwrap();
    assert_eq!(hold(&w), first);

    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    let second = hold(&w);
    assert_eq!(second.retries, 2);
    assert!(
        second
            .next_at
            .unwrap()
            .duration_since(Timestamp::now())
            .as_secs()
            > 200
    );
    restore(&f, MERGE);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Done);
    assert_eq!(source(&w).hold, None);
    assert_eq!(source(&w).runs, 1);
    assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
    assert_eq!(sent(&f, "POST", REVIEWS), 1);
    assert_eq!(sent(&f, "PUT", MERGE), 3);
}

#[test]
fn exhausted_or_disabled_retries_ask_a_person_whose_retry_starts_over() {
    let (mut w, f, id) = safe_run();
    limits(&mut w).max = 2;
    reopen(&mut w, &f);
    respond(&f, MERGE, 502);
    finish(&mut w);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    let held = hold(&w);
    assert_eq!(held.reason, HoldReason::DeliveryStopped);
    assert!(held.message.contains("after 2 automatic retries"));
    assert!(source(&w).paused);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);

    let task = w.orchestrator.retry_publication(&id, "human").unwrap();
    assert_eq!(task.state, TaskState::WaitingToRetry);
    assert_eq!(hold(&w).retries, 1);

    let (mut w, f, id) = safe_run();
    limits(&mut w).max = 0;
    reopen(&mut w, &f);
    respond(&f, MERGE, 503);
    finish(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

#[test]
fn a_missing_approval_is_posted_again_but_a_confirmed_one_never_is() {
    let (mut w, f, id) = safe_run();
    respond(&f, REVIEWS, 503);
    finish(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert!(hold(&w).message.ends_with("while approving"));
    assert!(f.state.lock().unwrap().reviews.is_empty());
    restore(&f, REVIEWS);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Done);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);

    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    f.state.lock().unwrap().reviews.clear();
    restore(&f, MERGE);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(f.state.lock().unwrap().reviews.is_empty());
    assert!(!f.state.lock().unwrap().merged);
}

#[test]
fn a_duplicate_approval_from_a_late_response_does_not_block_delivery() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    {
        let mut s = f.state.lock().unwrap();
        let mut copy = s.reviews[0].clone();
        copy["id"] = json!(2);
        s.reviews.push(copy);
    }
    restore(&f, MERGE);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Done);
}

#[test]
fn refusals_are_not_retried_and_rate_limits_wait_for_the_advised_time() {
    for status in [401, 403, 405, 422] {
        let (mut w, f, id) = safe_run();
        respond(&f, MERGE, status);
        finish(&mut w);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{status}");
    }
    let (mut w, f, id) = safe_run();
    let mut limited = Reply::new(403, json!({"message":"secondary rate limit"}));
    limited.headers.insert("retry-after".into(), "1800".into());
    f.state
        .lock()
        .unwrap()
        .overrides
        .insert(MERGE.into(), limited);
    finish(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    let wait = hold(&w)
        .next_at
        .unwrap()
        .duration_since(Timestamp::now())
        .as_secs();
    assert!(wait > 1700, "{wait}");
}

#[test]
fn pauses_objections_and_revoked_authority_stop_a_waiting_retry() {
    for reason in ["pause", "objection", "revoked", "checks", "close"] {
        let (mut w, f, id) = safe_run();
        respond(&f, MERGE, 503);
        finish(&mut w);
        restore(&f, MERGE);
        match reason {
            "pause" => {
                let tracked = source(&w).id;
                w.orchestrator
                    .pause_pull_request(&tracked, "human")
                    .unwrap();
                assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
            }
            "objection" => f.state.lock().unwrap().reviews.push(json!({
                "id": 90, "user": {"login": "maintainer", "type": "User"},
                "commit_id": "0".repeat(40), "body": "No.", "state": "CHANGES_REQUESTED"})),
            "revoked" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .effects
                    .merge_source_merge_request_on_safe_verdict = false;
            }
            "checks" => f.state.lock().unwrap().conclusion = "failure".into(),
            "close" => {
                w.orchestrator.close_task(&id, "human").unwrap();
            }
            _ => unreachable!(),
        }
        for _ in 0..2 {
            later(&mut w, &f);
        }
        assert_eq!(
            task_state(&w, &id),
            if reason == "close" {
                TaskState::Closed
            } else {
                TaskState::NeedsHuman
            },
            "{reason}"
        );
        assert!(source(&w).paused, "{reason}");
        assert_eq!(
            hold(&w).reason,
            match reason {
                "pause" | "close" => HoldReason::OperatorPause,
                "checks" => HoldReason::FailedChecks,
                _ => HoldReason::DeliveryStopped,
            },
            "{reason}"
        );
        assert!(!f.state.lock().unwrap().merged, "{reason}");
        assert_eq!(sent(&f, "PUT", MERGE), 1, "{reason}");
    }
}

#[test]
fn a_head_that_moves_while_waiting_is_assessed_afresh() {
    for lost in ["approval", "merge", "unrelated"] {
        let (mut w, f, id) = safe_run();
        match lost {
            "approval" => f.state.lock().unwrap().lose_review_response = true,
            "merge" => respond(&f, MERGE, 503),
            _ => respond(&f, REVIEWS, 503),
        }
        finish(&mut w);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
        let head = move_source_head(&w, &f);
        restore(&f, MERGE);
        restore(&f, REVIEWS);
        // Seeing the new head brings the retry forward.
        assert!(scan(&mut w).is_empty());
        assert!(hold(&w).next_at.unwrap() <= Timestamp::now());
        reopen(&mut w, &f);
        w.orchestrator
            .tick_at(Timestamp::now() + SignedDuration::from_secs(1))
            .unwrap();
        assert_eq!(task_state(&w, &id), TaskState::Closed, "{lost}");
        let tracked = source(&w);
        assert_eq!(tracked.hold, None);
        assert_eq!(tracked.runs, 2, "{lost}");
        let next = tracked.last_task_id.unwrap();
        assert_ne!(next, id);
        let next = w.orchestrator.db.get_task(&next).unwrap();
        assert_eq!(
            artidezi_orchestrator::discovery::source_of(&next)
                .unwrap()
                .unwrap()
                .input
                .commit,
            head
        );
        assert!(!f.state.lock().unwrap().merged, "{lost}");
    }
}

#[test]
fn a_lost_proposal_is_recovered_and_closed_when_the_head_moves() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().lose_create_response = true;
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    move_source_head(&w, &f);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(f.state.lock().unwrap().proposals[&100]["state"], "closed");
    assert_ne!(source(&w).last_task_id.as_deref(), Some(id.as_str()));

    // A proposal that someone edited is not ours to close.
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().lose_create_response = true;
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    f.state.lock().unwrap().proposals.get_mut(&100).unwrap()["title"] = json!("Edited");
    move_source_head(&w, &f);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(f.state.lock().unwrap().proposals[&100]["state"], "open");
}

#[test]
fn a_source_merged_by_someone_else_while_waiting_closes_the_task() {
    let (mut w, f, id) = safe_run();
    respond(&f, REVIEWS, 503);
    finish(&mut w);
    {
        let mut s = f.state.lock().unwrap();
        s.open = false;
        s.merged = true;
    }
    assert!(scan(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(source(&w).hold, None);
}

#[test]
fn a_question_for_a_person_is_still_delivered_once_after_a_temporary_failure() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().lose_review_response = true;
    let script = w.script(
        "question.sh",
        &result_writer(
            r#"{"schema_version":1,"outcome":"needs_human","summary":"Which version?","repositories":[{"id":"role","changed":false}],"questions_for_human":["Which version?"]}"#,
        ),
    );
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(hold(&w).reason, HoldReason::Question);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
}

#[test]
fn delivery_interrupted_by_a_restart_is_retried_within_the_same_limit() {
    let (mut w, f, id) = safe_run();
    limits(&mut w).max = 2;
    respond(&f, MERGE, 503);
    finish(&mut w);
    for _ in 0..2 {
        // A crash after leaving the waiting state, before any outcome was recorded.
        w.orchestrator
            .db
            .set_task_state(&id, TaskState::Working, "fixture")
            .unwrap();
        reopen(&mut w, &f);
    }
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(hold(&w).message.contains("interrupted by restart"));
}

#[test]
fn preparing_a_task_during_an_outage_does_not_stop_the_change() {
    let (mut w, f) = world();
    let branch = "/repos/org/app/branches/main";
    respond(&f, branch, 503);
    let report = w
        .orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(report.queued.is_empty() && report.errors.len() == 1);
    let tracked = source(&w);
    assert!(!tracked.paused);
    assert!(tracked.follow_up.is_some());
    restore(&f, branch);
    assert_eq!(scan(&mut w).len(), 1);
}

#[test]
fn a_temporary_or_interrupted_adaptation_merge_is_tried_again() {
    for cause in ["temporary", "interrupted"] {
        let (mut w, f) = adaptation_merge_world();
        limits(&mut w).initial_delay_seconds = 0;
        limits(&mut w).max = 2;
        reopen(&mut w, &f);
        let id = scan(&mut w).remove(0);
        assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
        let path = "/repos/org/app/pulls/100/merge";
        if cause == "temporary" {
            respond(&f, path, 503);
            w.orchestrator.reconcile_task(&id).unwrap();
            assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
            assert_eq!(hold(&w).reason, HoldReason::Delivery);
            assert!(!source(&w).paused);
            restore(&f, path);
        } else {
            let mr = w
                .orchestrator
                .db
                .list_merge_requests(&id)
                .unwrap()
                .remove(0);
            w.orchestrator
                .db
                .record_event(
                    Some(&id),
                    None,
                    "orchestrator",
                    "adaptation.merge_planned",
                    json!({"repository": mr.repository_id, "number": mr.number, "head": mr.head_commit}),
                )
                .unwrap();
        }
        w.orchestrator.reconcile_task(&id).unwrap();
        let _ = w.orchestrator.reconcile_task(&id);
        assert_eq!(task_state(&w, &id), TaskState::Done, "{cause}");
        assert_eq!(source(&w).hold, None, "{cause}");
    }

    // Waits between tries, and stops at the limit.
    let (mut w, f) = adaptation_merge_world();
    limits(&mut w).max = 1;
    reopen(&mut w, &f);
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    respond(&f, "/repos/org/app/pulls/100/merge", 503);
    w.orchestrator.reconcile_task(&id).unwrap();
    w.orchestrator.reconcile_task(&id).unwrap();
    assert_eq!(proposal_merges(&f), 1);
    limits(&mut w).initial_delay_seconds = 0;
    for _ in 0..3 {
        w.orchestrator.reconcile_task(&id).unwrap();
    }
    assert_eq!(proposal_merges(&f), 2);
    assert_eq!(source(&w).hold, None);
    assert!(source(&w).status.contains("did not complete"));
}

#[test]
fn holds_recorded_before_they_were_typed_are_adopted_only_when_certain() {
    for kind in ["failed_checks", "operator", "unknown"] {
        let (mut w, f, id) = safe_run();
        f.state.lock().unwrap().conclusion = "failure".into();
        finish(&mut w);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        // As an older controller stored it.
        let payload = if kind == "unknown" {
            json!({"reason": "source checks changed; evaluate again"})
        } else {
            json!({"reason": "Current forge checks failed", "recheck": "failed_checks"})
        };
        w.orchestrator
            .db
            .record_event(Some(&id), None, "orchestrator", "task.needs_human", payload)
            .unwrap();
        if kind == "operator" {
            w.orchestrator
                .db
                .record_event(Some(&id), None, "human", "discovery.paused", json!({}))
                .unwrap();
        }
        let mut tracked = source(&w);
        tracked.hold = None;
        w.orchestrator
            .db
            .save_pull_request_source(&tracked)
            .unwrap();
        f.state.lock().unwrap().conclusion = "success".into();
        reopen(&mut w, &f);
        assert_eq!(
            hold(&w).reason,
            match kind {
                "failed_checks" => HoldReason::FailedChecks,
                "operator" => HoldReason::OperatorPause,
                _ => HoldReason::Unclassified,
            }
        );
        later(&mut w, &f);
        assert_eq!(
            task_state(&w, &id),
            if kind == "failed_checks" {
                TaskState::Done
            } else {
                TaskState::NeedsHuman
            },
            "{kind}"
        );
    }
}
