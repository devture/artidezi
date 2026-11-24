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

/// A published adaptation whose assessment review failed, so that delivery waits.
fn published_adaptation() -> (World, Fixture, String) {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    respond(&f, REVIEWS, 503);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(w.orchestrator.db.list_merge_requests(&id).unwrap().len(), 1);
    restore(&f, REVIEWS);
    (w, f, id)
}

#[test]
fn a_proposal_that_someone_decided_on_or_edited_is_neither_pushed_nor_updated_again() {
    for decision in ["merged", "closed", "edited", "lost_then_closed"] {
        let (mut w, f, id) = if decision == "lost_then_closed" {
            let (mut w, f) = publishing_world();
            let id = scan(&mut w).remove(0);
            f.state.lock().unwrap().lose_create_response = true;
            assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
            (w, f, id)
        } else {
            published_adaptation()
        };
        let branch = format!("artidezi/{id}");
        {
            let mut s = f.state.lock().unwrap();
            let pr = s.proposals.get_mut(&100).unwrap();
            if decision == "edited" {
                pr["title"] = json!("A maintainer's title");
            } else {
                pr["state"] = json!("closed");
                pr["merged"] = json!(decision == "merged");
                // The fixture reports live branch heads; keep the one it merged or closed.
                s.branches.remove(&branch);
                w.forge
                    .ensure_repository("org/app")
                    .unwrap()
                    .find_reference(&format!("refs/heads/{branch}"))
                    .unwrap()
                    .delete()
                    .unwrap();
            }
        }
        later(&mut w, &f);
        assert_eq!(
            task_state(&w, &id),
            if decision == "merged" {
                // The review follows, and reconciliation in the same pass sees the merge.
                TaskState::Done
            } else {
                TaskState::NeedsHuman
            },
            "{decision}"
        );
        assert_eq!(
            w.forge.branch_head("org/app", &branch).is_ok(),
            decision == "edited",
            "{decision}"
        );
        let s = f.state.lock().unwrap();
        assert_eq!(s.proposals.len(), 1);
        if decision == "edited" {
            assert_eq!(s.proposals[&100]["title"], "A maintainer's title");
        }
    }
}

#[test]
fn a_routing_change_or_a_lost_hold_never_leaves_a_task_waiting_forever() {
    for cause in ["retargeted", "handler", "lost_hold", "half_a_pause"] {
        let (mut w, f, id) = safe_run();
        respond(&f, REVIEWS, 503);
        finish(&mut w);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
        let mut tracked = source(&w);
        match cause {
            "retargeted" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .repositories
                    .get_mut("role")
                    .unwrap()
                    .branch = "release".into();
            }
            "handler" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .pull_requests
                    .as_mut()
                    .unwrap()
                    .handlers[0]
                    .id = "renamed".into();
            }
            "lost_hold" => tracked.hold = None,
            _ => tracked.place_hold(Hold::new(HoldReason::OperatorPause, "paused")),
        }
        w.orchestrator
            .db
            .save_pull_request_source(&tracked)
            .unwrap();
        reopen(&mut w, &f);
        let _ = w.orchestrator.scan_pull_requests(Some("roles"), true);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry, "{cause}");
        later(&mut w, &f);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{cause}");
        assert_eq!(
            hold(&w).reason,
            if cause == "half_a_pause" {
                HoldReason::OperatorPause
            } else {
                HoldReason::DeliveryStopped
            },
            "{cause}"
        );
        assert!(f.state.lock().unwrap().reviews.is_empty());
    }
}

#[test]
fn a_temporary_error_on_reopening_keeps_the_gate_for_open_adaptations() {
    let (mut w, f) = publishing_world();
    scan(&mut w);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    f.state.lock().unwrap().open = false;
    assert!(scan(&mut w).is_empty());
    move_source_head(&w, &f);
    f.state.lock().unwrap().open = true;
    let listing = "/repos/org/app/pulls/100";
    respond(&f, listing, 503);
    for outage in [true, false] {
        if !outage {
            restore(&f, listing);
        }
        let report = w
            .orchestrator
            .scan_pull_requests(Some("roles"), true)
            .unwrap();
        assert!(report.queued.is_empty(), "{outage}");
        assert_eq!(report.errors.len(), 1, "{outage}");
        assert_eq!(source(&w).paused, !outage);
    }
}

#[test]
fn a_moved_head_brings_a_waiting_delivery_forward_only_once() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    move_source_head(&w, &f);
    let reviews = format!("{REVIEWS}?per_page=100&page=1");
    respond(&f, &reviews, 503);
    for _ in 0..6 {
        let _ = w.orchestrator.scan_pull_requests(Some("roles"), true);
        w.orchestrator
            .tick_at(Timestamp::now() + SignedDuration::from_secs(1))
            .unwrap();
    }
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(hold(&w).retries, 2);
}

#[test]
fn waiting_for_a_known_backoff_uses_no_retry_and_then_shows_the_real_refusal() {
    let (mut w, f, id) = safe_run();
    limits(&mut w).max = 2;
    respond(&f, MERGE, 503);
    finish(&mut w);
    assert_eq!(hold(&w).retries, 1);
    w.orchestrator
        .db
        .defer_forge("github", Timestamp::now() + SignedDuration::from_secs(2))
        .unwrap();
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    assert_eq!(hold(&w).retries, 1);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    respond(&f, MERGE, 403);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);

    // A backoff that never ends is bounded too.
    let (mut w, f, id) = safe_run();
    limits(&mut w).max = 2;
    respond(&f, MERGE, 503);
    finish(&mut w);
    w.orchestrator
        .db
        .defer_forge("github", Timestamp::now() + SignedDuration::from_hours(1))
        .unwrap();
    for _ in 0..4 {
        later(&mut w, &f);
    }
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
}

#[test]
fn an_unrelated_review_of_the_bot_does_not_block_a_retry_that_never_tried_to_review() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    let head = f.state.lock().unwrap().head.clone();
    f.state.lock().unwrap().reviews.push(json!({
        "id": 70, "user": {"login": "artidezi-fixture[bot]", "type": "Bot"},
        "commit_id": head, "body": "An inline reply.", "state": "COMMENTED"}));
    f.state.lock().unwrap().lose_create_response = true;
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
}

#[test]
fn a_waiting_task_replaced_by_a_newer_evaluation_does_not_stop_it() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    // As after a crash between queuing a newer evaluation and closing this task.
    let mut newer = w.orchestrator.db.get_task(&id).unwrap();
    newer.id = artidezi_store::new_id("tsk");
    newer.number = 0;
    newer.state = TaskState::Done;
    w.orchestrator.db.insert_task(&newer).unwrap();
    let mut tracked = source(&w);
    tracked.last_task_id = Some(newer.id);
    tracked.hold = None;
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert!(!source(&w).paused);
}

#[test]
fn a_lost_comment_review_that_someone_edited_or_deleted_is_not_posted_again() {
    for change in ["edited", "deleted_after_confirmation"] {
        let (mut w, f) = publishing_world();
        let id = scan(&mut w).remove(0);
        f.state.lock().unwrap().lose_review_response = true;
        assert_eq!(w.work(&unchanged(&w)), AttemptState::Valid);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
        {
            let mut s = f.state.lock().unwrap();
            if change == "edited" {
                s.reviews[0]["body"] = json!("Edited");
            } else {
                s.reviews.clear();
                drop(s);
                let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap().id;
                w.orchestrator
                    .db
                    .record_event(
                        Some(&id),
                        Some(&attempt),
                        "orchestrator",
                        "assessment.published",
                        json!({"state": "commented"}),
                    )
                    .unwrap();
            }
        }
        later(&mut w, &f);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{change}");
        assert_eq!(sent(&f, "POST", REVIEWS), 1, "{change}");
    }
}

#[test]
fn a_backoff_does_not_hold_published_adaptations_for_a_person() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    w.orchestrator
        .db
        .defer_forge("github", Timestamp::now() + SignedDuration::from_hours(1))
        .unwrap();
    reopen(&mut w, &f);
    w.orchestrator.tick().unwrap();
    assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
    assert!(!source(&w).paused);
}

#[test]
fn a_delivery_hold_of_an_adaptation_merge_ends_with_the_source_or_the_permission() {
    for end in ["closed", "revoked"] {
        let (mut w, f) = adaptation_merge_world();
        let id = scan(&mut w).remove(0);
        assert_eq!(w.work(&proposal(&w)), AttemptState::Valid);
        respond(&f, "/repos/org/app/pulls/100/merge", 503);
        w.orchestrator.reconcile_task(&id).unwrap();
        assert_eq!(hold(&w).reason, HoldReason::Delivery);
        if end == "closed" {
            f.state.lock().unwrap().open = false;
            assert!(scan(&mut w).is_empty());
        } else {
            w.orchestrator
                .config
                .projects
                .get_mut("roles")
                .unwrap()
                .pull_requests
                .as_mut()
                .unwrap()
                .handlers[0]
                .adaptation_action = Default::default();
            reopen(&mut w, &f);
            w.orchestrator.reconcile_task(&id).unwrap();
        }
        assert_eq!(source(&w).hold, None, "{end}");
    }
}

#[test]
fn a_revision_that_crashed_before_its_state_change_replaces_the_retry() {
    let (mut w, f, id) = published_adaptation();
    w.orchestrator
        .revise_proposal(&id, "Rename it", "human")
        .unwrap();
    assert_eq!(source(&w).hold, None);
    w.orchestrator
        .db
        .set_task_state(&id, TaskState::WaitingToRetry, "fixture")
        .unwrap();
    let mut tracked = source(&w);
    let mut waiting = Hold::new(HoldReason::Delivery, "unavailable");
    waiting.task_id = Some(id.clone());
    waiting.next_at = Some(Timestamp::now());
    tracked.place_hold(waiting);
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Working);
    assert_eq!(source(&w).hold, None);
}

#[test]
fn switching_retries_off_stops_the_one_already_scheduled() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    restore(&f, MERGE);
    limits(&mut w).max = 0;
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(sent(&f, "PUT", MERGE), 1);
    // A later head is still evaluated.
    move_source_head(&w, &f);
    scan(&mut w);
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::Closed);

    // A lowered limit counts for the one already scheduled.
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    later(&mut w, &f);
    assert_eq!(hold(&w).retries, 2);
    limits(&mut w).max = 1;
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert_eq!(sent(&f, "PUT", MERGE), 2);
}

#[test]
fn an_objection_while_waiting_keeps_a_moved_head_with_a_person() {
    // Before anything was planned, and after an approval that arrived although its response did not.
    for case in ["unplanned", "lost_then_edited", "lost_then_stripped"] {
        let (mut w, f, id) = safe_run();
        match case {
            "unplanned" => respond(&f, REVIEWS, 503),
            _ => f.state.lock().unwrap().lose_review_response = true,
        }
        finish(&mut w);
        assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
        {
            let mut state = f.state.lock().unwrap();
            match case {
                "unplanned" => {
                    let head = state.head.clone();
                    state.reviews.push(json!({"id": 900, "user": {"login": "maintainer", "type": "User"}, "commit_id": head, "body": "No.", "state": "CHANGES_REQUESTED"}));
                }
                "lost_then_edited" => {
                    let body = state.reviews[0]["body"].as_str().unwrap().to_owned();
                    let marker = &body[body.find("<!-- artidezi:assessment:").unwrap()..];
                    state.reviews[0]["body"] = json!(format!("Not yet. {marker}"));
                }
                _ => state.reviews[0]["body"] = json!("Not yet."),
            }
        }
        restore(&f, REVIEWS);
        move_source_head(&w, &f);
        scan(&mut w);
        later(&mut w, &f);
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{case}");
        later(&mut w, &f);
        assert_eq!(source(&w).runs, 1, "{case}");
    }
}

#[test]
fn a_reopened_change_stays_paused_by_a_person() {
    let (mut w, f, id) = safe_run();
    respond(&f, MERGE, 503);
    finish(&mut w);
    let tracked = source(&w);
    w.orchestrator
        .pause_pull_request(&tracked.id, "operator")
        .unwrap();
    f.state.lock().unwrap().open = false;
    scan(&mut w);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    move_source_head(&w, &f);
    f.state.lock().unwrap().open = true;
    assert!(scan(&mut w).is_empty());
    assert_eq!(hold(&w).reason, HoldReason::OperatorPause);
}

#[test]
fn a_waiting_proposal_is_not_published_past_requested_changes() {
    let (mut w, f) = publishing_world();
    let id = scan(&mut w).remove(0);
    respond(&f, "/repos/org/app/pulls", 503);
    w.work(&proposal(&w));
    assert_eq!(task_state(&w, &id), TaskState::WaitingToRetry);
    restore(&f, "/repos/org/app/pulls");
    let head = source(&w).observed_head;
    f.state.lock().unwrap().reviews.push(json!({"id": 900, "user": {"login": "maintainer", "type": "User"}, "commit_id": head, "body": "No.", "state": "CHANGES_REQUESTED"}));
    later(&mut w, &f);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(f.state.lock().unwrap().proposals.is_empty());
}
