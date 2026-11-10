// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use chrono::{Duration, Utc};

#[test]
fn source_rebased_before_publication_gets_a_fresh_assessment_after_settling() {
    for adaptation in [false, true] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        let old_task = w.orchestrator.db.get_task(&id).unwrap();
        let new_head = move_source_head(&w, &f);
        w.orchestrator
            .config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .settle_seconds = 10;
        let script = if adaptation {
            proposal(&w)
        } else {
            safe_result(&w, Some("low"), true)
        };
        assert_eq!(w.work(&script), AttemptState::Valid);
        assert_eq!(task_state(&w, &id), TaskState::Closed);
        no_publication(&f);
        let tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        assert!(!tracked.paused);
        assert_eq!(
            tracked.runs, 1,
            "superseding never refunds or renews the budget"
        );
        assert_eq!(tracked.observed_head, new_head.0);
        let deadline = tracked.follow_up.unwrap().at;
        assert_eq!(deadline, tracked.head_observed_at + Duration::seconds(10));
        assert!(
            w.orchestrator
                .db
                .list_events(&id)
                .unwrap()
                .iter()
                .any(|event| event.kind == "assessment.superseded")
        );

        reopen(&mut w, &f);
        assert!(
            w.orchestrator
                .scan_pull_requests_at(Some("roles"), true, deadline - Duration::milliseconds(1))
                .unwrap()
                .queued
                .is_empty()
        );
        let report = w.orchestrator.tick_at(deadline).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.discovered.len(), 1);
        let next = &report.discovered[0];
        assert_ne!(next, &id);
        let next_task = w.orchestrator.db.get_task(next).unwrap();
        assert_eq!(
            artidezi_orchestrator::discovery::source_of(&next_task)
                .unwrap()
                .unwrap()
                .input
                .commit,
            new_head
        );
        assert_eq!(
            w.orchestrator.db.get_task(&id).unwrap().policy_snapshot,
            old_task.policy_snapshot
        );
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
            2
        );
        assert_eq!(w.work(&script), AttemptState::Valid);
        assert_eq!(
            task_state(&w, next),
            if adaptation {
                TaskState::AwaitingReview
            } else {
                TaskState::Done
            }
        );
        let state = f.state.lock().unwrap();
        assert_eq!(state.reviews.len(), 1);
        assert_eq!(state.reviews[0]["commit_id"], new_head.0);
        assert_eq!(
            state.merged, !adaptation,
            "adaptations still require human merge"
        );
    }
}

#[test]
fn repeated_rebases_exhaust_the_existing_budget_without_publishing() {
    let (mut w, f) = safe_world(true);
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .max_runs_per_pull_request = 2;
    let first = scan(&mut w).remove(0);
    move_source_head(&w, &f);
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &first), TaskState::Closed);
    let second = scan(&mut w).remove(0);
    // Produce a distinct second rebase, rather than recreating the first commit.
    let repo = w.forge.ensure_repository("org/app").unwrap();
    let base = repo.refname_to_id("refs/heads/main").unwrap();
    let head = seed(
        &repo,
        Some(base),
        "defaults.yml",
        "version: 3.0.0\n",
        "Another rebase",
    );
    repo.reference("refs/heads/main", base, true, "restore target")
        .unwrap();
    repo.reference("refs/pull/33/head", head, true, "new source")
        .unwrap();
    f.state.lock().unwrap().head = head.to_string();
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &second), TaskState::Closed);
    let tracked = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    assert_eq!(tracked.runs, 2);
    assert!(tracked.paused);
    assert!(tracked.follow_up.is_none());
    assert!(scan(&mut w).is_empty());
    no_publication(&f);
}

#[test]
fn unpublished_ci_wait_reconciles_a_new_head_before_queuing_fresh_work() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().conclusion = "running".into();
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    let head = move_source_head(&w, &f);
    f.state.lock().unwrap().conclusion = "success".into();
    // Discovery can notice the new head before the scheduled delivery check is due.
    assert!(scan(&mut w).is_empty());
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    assert!(!w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused);
    reopen(&mut w, &f);
    let report = w
        .orchestrator
        .tick_at(Utc::now() + Duration::seconds(601))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    assert_eq!(report.discovered.len(), 1);
    let fresh = w.orchestrator.db.get_task(&report.discovered[0]).unwrap();
    assert_eq!(
        artidezi_orchestrator::discovery::source_of(&fresh)
            .unwrap()
            .unwrap()
            .input
            .commit,
        head
    );
    no_publication(&f);
}

#[test]
fn rebases_after_an_approval_still_require_explicit_recovery() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    f.state.lock().unwrap().pending_on_approval = true;
    assert_eq!(
        w.work(&safe_result(&w, Some("low"), true)),
        AttemptState::Valid
    );
    assert_eq!(task_state(&w, &id), TaskState::WaitingForCi);
    assert_eq!(f.state.lock().unwrap().reviews.len(), 1);
    move_source_head(&w, &f);
    f.state.lock().unwrap().conclusion = "success".into();
    w.orchestrator
        .tick_at(Utc::now() + Duration::seconds(601))
        .unwrap();
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    assert!(w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused);
    assert!(scan(&mut w).is_empty());
    let state = f.state.lock().unwrap();
    assert_eq!(state.reviews.len(), 1);
    assert!(!state.merged);
}

#[test]
fn a_rebase_does_not_override_other_holds_or_potential_remote_effects() {
    for reason in [
        "draft",
        "title",
        "needs_human",
        "failed",
        "proposal_plan",
        "approval_plan",
    ] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        move_source_head(&w, &f);
        let script = match reason {
            "needs_human" => w.script("needs-human.sh", &result_writer(r#"{"schema_version":1,"outcome":"needs_human","summary":"Maintainer decision required.","questions_for_human":["Which behavior should be preserved?"],"repositories":[{"id":"role","changed":false}]}"#)),
            "failed" => w.script("failed.sh", &result_writer(r#"{"schema_version":1,"outcome":"failed","summary":"Assessment failed.","repositories":[{"id":"role","changed":false}]}"#)),
            _ => safe_result(&w, Some("low"), true),
        };
        match reason {
            "draft" => f.state.lock().unwrap().draft = true,
            "title" => f.state.lock().unwrap().title = "Different source".into(),
            "proposal_plan" | "approval_plan" => {
                let event = match reason {
                    "proposal_plan" => "publication.planned",
                    "approval_plan" => "assessment.approval_planned",
                    _ => unreachable!(),
                };
                w.orchestrator
                    .db
                    .record_event(Some(&id), None, "fixture", event, json!({}))
                    .unwrap();
            }
            _ => {}
        }
        assert_eq!(w.work(&script), AttemptState::Valid, "{reason}");
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{reason}");
        assert!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused,
            "{reason}"
        );
        assert!(scan(&mut w).is_empty(), "{reason}");
        no_publication(&f);
    }
}

#[test]
fn stale_held_assessment_self_heals_after_a_retry_that_never_started_publication() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let script = safe_result(&w, Some("low"), true);
    // A human pause blocks delivery while the original worker is running.
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    // Simulate a pause after leasing: pausing queued work would cancel it instead.
    let mut paused = source.clone();
    paused.paused = true;
    w.orchestrator.db.save_pull_request_source(&paused).unwrap();
    w.orchestrator
        .db
        .record_event(Some(&id), None, "operator", "discovery.paused", json!({}))
        .unwrap();
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
    move_source_head(&w, &f);
    // Explicit publication retry supersedes that pause, but performs no forge writes when stale.
    let outcome = w.orchestrator.retry_publication(&id, "operator").unwrap();
    assert_eq!(outcome.state, TaskState::Closed);
    let tracked = w
        .orchestrator
        .db
        .get_pull_request_source(&source.id)
        .unwrap();
    assert!(!tracked.paused);
    assert!(tracked.follow_up.is_some());
    no_publication(&f);
}

#[test]
fn old_controller_stale_hold_is_adopted_automatically_without_remote_effects() {
    let (mut w, f) = safe_world(true);
    let id = scan(&mut w).remove(0);
    let source = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    // Simulate a pause after leasing: pausing queued work would cancel it instead.
    let mut paused = source.clone();
    paused.paused = true;
    w.orchestrator.db.save_pull_request_source(&paused).unwrap();
    w.orchestrator
        .db
        .record_event(Some(&id), None, "operator", "discovery.paused", json!({}))
        .unwrap();
    let script = safe_result(&w, Some("low"), true);
    assert_eq!(w.work(&script), AttemptState::Valid);
    move_source_head(&w, &f);
    let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
    w.orchestrator
        .db
        .record_event(
            Some(&id),
            Some(&attempt.id),
            "operator",
            "publication.retry_requested",
            json!({}),
        )
        .unwrap();
    w.orchestrator.db.record_event(Some(&id),Some(&attempt.id),"orchestrator","task.needs_human",json!({"reason":"publication retry withheld: source head changed from old to new; a fresh evaluation is required"})).unwrap();
    let report = w.orchestrator.tick_at(Utc::now()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    no_publication(&f);
}

#[test]
fn transient_hold_on_an_obsolete_head_uses_fresh_inputs_without_bypassing_other_holds() {
    for case in ["recover", "operator_pause", "budget", "publication_plan"] {
        let (mut w, f) = safe_world(true);
        w.orchestrator
            .config
            .agent_profiles
            .get_mut("fake")
            .unwrap()
            .harness = artidezi_contract::job::Harness::Codex;
        w.orchestrator
            .config
            .worker_pools
            .get_mut("personal")
            .unwrap()
            .harnesses = vec![artidezi_contract::job::Harness::Codex];
        let endpoint = w
            .orchestrator
            .config
            .model_endpoints
            .get_mut("fake")
            .unwrap();
        endpoint.provider = artidezi_config::model::ModelProvider::Openai;
        endpoint.auth_mode = "chatgpt_login".into();
        let id = scan(&mut w).remove(0);
        let script = w.script("capacity.sh", r#"printf '%s\n' '{"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}'
exit 1"#);
        assert_eq!(
            w.work_with_record(&script, |record| record.harness.name = "codex".into()),
            AttemptState::Invalid
        );
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman);
        let head = move_source_head(&w, &f);
        assert!(scan(&mut w).is_empty());
        let mut tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        assert_eq!(tracked.observed_head, head.0);
        match case {
            "operator_pause" => {
                w.orchestrator
                    .pause_pull_request(&tracked.id, "operator")
                    .unwrap();
            }
            "budget" => {
                tracked.runs = 3;
                w.orchestrator
                    .db
                    .save_pull_request_source(&tracked)
                    .unwrap();
            }
            "publication_plan" => {
                w.orchestrator
                    .db
                    .record_event(Some(&id), None, "fixture", "publication.planned", json!({}))
                    .unwrap();
            }
            _ => {}
        }
        w.orchestrator
            .config
            .worker_pools
            .get_mut("personal")
            .unwrap()
            .transient_failure_retries = 2;
        let report = w
            .orchestrator
            .tick_at(Utc::now() + Duration::seconds(1))
            .unwrap();
        assert!(report.errors.is_empty(), "{case}: {:?}", report.errors);
        assert_eq!(
            w.orchestrator.db.list_attempts(&id).unwrap().len(),
            1,
            "never rerun stale inputs"
        );
        if matches!(case, "recover" | "budget") {
            assert_eq!(task_state(&w, &id), TaskState::Closed, "{case}");
        } else {
            assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{case}");
        }
        if case == "recover" {
            assert_eq!(report.discovered.len(), 1);
            let fresh = w.orchestrator.db.get_task(&report.discovered[0]).unwrap();
            assert_eq!(
                artidezi_orchestrator::discovery::source_of(&fresh)
                    .unwrap()
                    .unwrap()
                    .input
                    .commit,
                head
            );
            assert_eq!(
                w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
                2
            );
        } else {
            assert!(report.discovered.is_empty(), "{case}");
            assert!(
                w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused,
                "{case}"
            );
        }
        no_publication(&f);
    }
}

#[test]
fn new_transient_failures_respect_operator_pause_and_newer_source_inputs() {
    for changed in [false, true] {
        for paused in [false, true] {
            let (mut w, f) = safe_world(true);
            w.orchestrator
                .config
                .agent_profiles
                .get_mut("fake")
                .unwrap()
                .harness = artidezi_contract::job::Harness::Codex;
            let pool = w
                .orchestrator
                .config
                .worker_pools
                .get_mut("personal")
                .unwrap();
            pool.harnesses = vec![artidezi_contract::job::Harness::Codex];
            pool.transient_failure_retries = 2;
            let endpoint = w
                .orchestrator
                .config
                .model_endpoints
                .get_mut("fake")
                .unwrap();
            endpoint.provider = artidezi_config::model::ModelProvider::Openai;
            endpoint.auth_mode = "chatgpt_login".into();
            let id = scan(&mut w).remove(0);
            let mut tracked = w
                .orchestrator
                .db
                .list_pull_request_sources(None)
                .unwrap()
                .remove(0);
            if changed {
                tracked.observed_head = move_source_head(&w, &f).0;
                tracked.head_observed_at = Utc::now();
            }
            tracked.paused = paused;
            let db = w.orchestrator.db.clone();
            let script = w.script("capacity.sh", r#"printf '%s\n' '{"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}'
exit 1"#);
            // Apply operator/discovery updates after the worker leases, before its final upload.
            assert_eq!(
                w.work_with_record(&script, |record| {
                    record.harness.name = "codex".into();
                    db.save_pull_request_source(&tracked).unwrap();
                    if paused {
                        db.record_event(Some(&id), None, "operator", "discovery.paused", json!({}))
                            .unwrap();
                    }
                }),
                AttemptState::Invalid
            );
            let expected = if paused {
                TaskState::NeedsHuman
            } else if changed {
                TaskState::Closed
            } else {
                TaskState::Working
            };
            assert_eq!(
                task_state(&w, &id),
                expected,
                "changed={changed} paused={paused}"
            );
            assert_eq!(
                w.orchestrator.db.list_attempts(&id).unwrap().len(),
                if !changed && !paused { 2 } else { 1 }
            );
            no_publication(&f);
        }
    }
}

#[test]
fn a_damaged_held_attempt_does_not_stop_unrelated_automatic_discovery() {
    let (mut w, f) = safe_world(true);
    let fixture = artidezi_config::parse(crate::CONFIG).unwrap();
    w.orchestrator
        .config
        .projects
        .insert("manual-fixture".into(), fixture.projects["roles"].clone());
    let bad = w
        .orchestrator
        .create_manual_task(
            "manual-fixture",
            "No result fixture",
            "operator",
            &Default::default(),
        )
        .unwrap();
    let script = w.script("no-result.sh", "exit 1");
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let attempt = w.orchestrator.db.latest_attempt(&bad.id).unwrap().unwrap();
    let reference = attempt.attempt_record.unwrap();
    std::fs::remove_file(w.orchestrator.artifacts.path(&reference).unwrap()).unwrap();
    w.orchestrator
        .config
        .worker_pools
        .get_mut("personal")
        .unwrap()
        .transient_failure_retries = 2;
    let report = w.orchestrator.tick().unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains(&bad.id));
    assert_eq!(report.discovered.len(), 1);
    assert_eq!(task_state(&w, &bad.id), TaskState::NeedsHuman);
    no_publication(&f);
}

fn published_adaptation(w: &mut World) -> String {
    let id = scan(w).remove(0);
    assert_eq!(w.work(&proposal(w)), AttemptState::Valid);
    assert_eq!(task_state(w, &id), TaskState::AwaitingReview);
    assert_eq!(w.orchestrator.db.list_merge_requests(&id).unwrap().len(), 1);
    id
}

/// Recreates the hold an earlier release left when the source moved under a published adaptation.
fn hold_as_an_earlier_release_did(w: &mut World, id: &str) {
    let mut tracked = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    tracked.paused = true;
    tracked.status = "Source changed while adaptations await review".into();
    w.orchestrator
        .db
        .save_pull_request_source(&tracked)
        .unwrap();
    w.orchestrator
        .db
        .set_task_state(id, TaskState::NeedsHuman, "discovery")
        .unwrap();
}

fn source_untouched(f: &Fixture) {
    let state = f.state.lock().unwrap();
    assert!(state.open && !state.merged);
    assert_eq!(
        state.reviews.len(),
        1,
        "only the original assessment review"
    );
    assert!(state.requests.iter().all(|r| r.method == "GET"
        || !r.path.starts_with("/repos/org/app/pulls/33")
        || r.path == "/repos/org/app/pulls/33/reviews"));
}

#[test]
fn published_adaptation_for_a_moved_source_is_closed_and_reassessed_within_budget() {
    for held in [false, true] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        w.orchestrator
            .config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .settle_seconds = 10;
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        let new_head = move_source_head(&w, &f);
        if held {
            hold_as_an_earlier_release_did(&mut w, &id);
            assert!(scan(&mut w).is_empty());
        }
        reopen(&mut w, &f);
        let report = w.orchestrator.tick().unwrap();
        assert!(report.errors.is_empty(), "held={held}: {:?}", report.errors);
        assert!(report.discovered.is_empty(), "the new head settles first");
        assert_eq!(task_state(&w, &id), TaskState::Closed, "held={held}");

        let state = f.state.lock().unwrap();
        let proposal = &state.proposals[&published.number];
        assert_eq!(proposal["state"], "closed");
        assert_eq!(proposal["merged"], false);
        assert_eq!(state.comments.len(), 1);
        assert_eq!(state.comments[0]["number"], published.number);
        assert!(
            state.comments[0]["body"]
                .as_str()
                .unwrap()
                .contains(&new_head.0)
        );
        drop(state);
        source_untouched(&f);
        let closed = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        assert_eq!(closed.state, MergeRequestState::Closed);
        let events = w.orchestrator.db.list_events(&id).unwrap();
        assert!(events.iter().any(|e| e.kind == "merge_request.superseded"
            && e.payload["number"] == published.number
            && e.payload["source_head"] == new_head.0));
        assert!(
            events.iter().any(|e| e.kind == "assessment.superseded"
                && e.payload["cause"] == "published_adaptation")
        );
        let tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        assert!(!tracked.paused);
        assert_eq!(
            tracked.runs, 1,
            "superseding never refunds or renews the budget"
        );
        let deadline = tracked.follow_up.unwrap().at;

        let report = w.orchestrator.tick_at(deadline).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.discovered.len(), 1);
        let next = w.orchestrator.db.get_task(&report.discovered[0]).unwrap();
        assert_eq!(
            artidezi_orchestrator::discovery::source_of(&next)
                .unwrap()
                .unwrap()
                .input
                .commit,
            new_head
        );
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
            2
        );
        assert_eq!(f.state.lock().unwrap().comments.len(), 1);
    }
}

#[test]
fn current_changed_or_paused_adaptations_are_never_closed_automatically() {
    for case in ["current", "pushed", "foreign", "paused"] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        match case {
            "pushed" => {
                f.state
                    .lock()
                    .unwrap()
                    .branches
                    .insert(published.source_branch.clone(), "f".repeat(40));
            }
            "foreign" => {
                f.state
                    .lock()
                    .unwrap()
                    .proposals
                    .get_mut(&published.number)
                    .unwrap()["user"]["login"] = json!("someone-else");
            }
            "paused" => {
                let tracked = w.orchestrator.db.list_pull_request_sources(None).unwrap();
                w.orchestrator
                    .pause_pull_request(&tracked[0].id, "operator")
                    .unwrap();
            }
            _ => {}
        }
        if case == "current" {
            hold_as_an_earlier_release_did(&mut w, &id);
        } else {
            move_source_head(&w, &f);
        }
        for _ in 0..2 {
            scan(&mut w);
            w.orchestrator.tick().unwrap();
        }
        assert_eq!(task_state(&w, &id), TaskState::NeedsHuman, "{case}");
        let state = f.state.lock().unwrap();
        assert_eq!(
            state.proposals[&published.number]["state"], "open",
            "{case}"
        );
        assert!(state.comments.is_empty(), "{case}");
        drop(state);
        source_untouched(&f);
        assert!(
            w.orchestrator
                .db
                .list_events(&id)
                .unwrap()
                .iter()
                .all(|e| !e.kind.starts_with("merge_request.close")
                    && e.kind != "assessment.superseded"),
            "{case}"
        );
        assert!(w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].paused);
        if case == "current" {
            let tracked = w.orchestrator.db.list_pull_request_sources(None).unwrap();
            let error = w
                .orchestrator
                .resume_pull_request(&tracked[0].id, "human")
                .unwrap_err();
            assert!(
                error.to_string().contains("existing adaptation proposals"),
                "{error}"
            );
        }
    }
}

#[test]
fn resume_closes_an_adaptation_for_an_obsolete_source_head_instead_of_refusing() {
    let (mut w, f) = publishing_world();
    let id = published_adaptation(&mut w);
    let published = w
        .orchestrator
        .db
        .list_merge_requests(&id)
        .unwrap()
        .remove(0);
    let tracked = w
        .orchestrator
        .db
        .list_pull_request_sources(None)
        .unwrap()
        .remove(0);
    w.orchestrator
        .pause_pull_request(&tracked.id, "operator")
        .unwrap();
    move_source_head(&w, &f);
    scan(&mut w);
    let resumed = w
        .orchestrator
        .resume_pull_request(&tracked.id, "human")
        .unwrap();
    assert!(!resumed.paused);
    assert_eq!(resumed.runs, 0, "an explicit resume renews the budget");
    assert_eq!(task_state(&w, &id), TaskState::Closed);
    let state = f.state.lock().unwrap();
    assert_eq!(state.proposals[&published.number]["state"], "closed");
    assert_eq!(state.comments.len(), 1);
    drop(state);
    source_untouched(&f);
    assert_eq!(scan(&mut w).len(), 1);
}
