// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use chrono::{Duration, Utc};

/// Advance an unrelated target file, then replay the source's exact change on that target.
pub(super) fn rebase_source(w: &World, f: &Fixture) -> (CommitSha, CommitSha) {
    let repo = w.forge.ensure_repository("org/app").unwrap();
    let old_head = Oid::from_str(&f.state.lock().unwrap().head).unwrap();
    let old_tree = repo.find_commit(old_head).unwrap().tree().unwrap();
    let content = old_tree
        .get_path(std::path::Path::new("defaults.yml"))
        .unwrap();
    let blob = repo.find_blob(content.id()).unwrap();
    let base = repo.refname_to_id("refs/heads/main").unwrap();
    let target = seed(
        &repo,
        Some(base),
        "target-note.txt",
        &format!("Target after {base}\n"),
        "Advance target",
    );
    let head = seed(
        &repo,
        Some(target),
        "defaults.yml",
        std::str::from_utf8(blob.content()).unwrap(),
        "Rebase source",
    );
    repo.reference("refs/heads/main", target, true, "restore target")
        .unwrap();
    repo.reference("refs/pull/33/head", head, true, "rebased source")
        .unwrap();
    f.state.lock().unwrap().head = head.to_string();
    (CommitSha(target.to_string()), CommitSha(head.to_string()))
}

#[test]
fn source_rebased_before_publication_gets_a_fresh_assessment_after_settling() {
    for adaptation in [false, true] {
        let (mut w, f) = safe_world(true);
        let id = scan(&mut w).remove(0);
        let old_task = w.orchestrator.db.get_task(&id).unwrap();
        let (_, new_head) = rebase_source(&w, &f);
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
    let (_, head) = rebase_source(&w, &f);
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
fn equivalent_rebases_preserve_published_adaptations_and_completed_delivery() {
    for held in [false, true] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let frozen = w.orchestrator.db.get_task(&id).unwrap();
        let frozen_source = artidezi_orchestrator::discovery::source_of(&frozen)
            .unwrap()
            .unwrap();
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        let repo = w.forge.ensure_repository("org/app").unwrap();
        let branch = format!("refs/heads/{}", published.source_branch);
        let branch_head = repo.refname_to_id(&branch).unwrap();
        // Preservation still works at the run limit and never renews or refunds it.
        w.orchestrator
            .config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .max_runs_per_pull_request = 1;
        let (fresh_target, observed_head) = rebase_source(&w, &f);
        if held {
            hold_as_an_earlier_release_did(&mut w, &id);
        }
        for _ in 0..3 {
            assert!(scan(&mut w).is_empty());
            let report = w.orchestrator.tick().unwrap();
            assert!(report.errors.is_empty(), "held={held}: {:?}", report.errors);
            assert!(report.discovered.is_empty());
            assert_eq!(task_state(&w, &id), TaskState::AwaitingReview);
            reopen(&mut w, &f);
        }
        let tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        assert!(!tracked.paused);
        assert_eq!(tracked.runs, 1);
        assert_eq!(tracked.status, "Published adaptations await review");
        assert!(tracked.follow_up.is_none());
        assert_eq!(w.orchestrator.db.list_tasks(None).unwrap().len(), 1);
        assert_eq!(w.orchestrator.db.list_attempts(&id).unwrap().len(), 1);
        assert_eq!(
            w.orchestrator.db.get_task(&id).unwrap().policy_snapshot,
            frozen.policy_snapshot
        );
        assert_eq!(repo.refname_to_id(&branch).unwrap(), branch_head);
        let events = w.orchestrator.db.list_events(&id).unwrap();
        let observed: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "source.equivalent_head_observed")
            .collect();
        assert_eq!(
            observed.len(),
            1,
            "one durable observation across scans and restarts"
        );
        assert_eq!(
            observed[0].payload,
            json!({"task":id,"frozen_head":frozen_source.input.commit,"observed_head":observed_head,"frozen_target":frozen.repositories.iter().find(|r| r.repository_id == "role").unwrap().base_commit,"fresh_target":fresh_target})
        );
        assert!(events.iter().all(|e| e.kind != "assessment.superseded"
            && e.kind != "merge_request.close_planned"
            && !e.kind.starts_with("merge_request.branch_delet")));
        assert!(
            events
                .iter()
                .filter(|e| e.kind == "assessment.published")
                .all(|e| e.payload["head"] != observed_head.0)
        );
        source_untouched(&f);
        assert_eq!(
            f.state.lock().unwrap().proposals[&published.number]["state"],
            "open"
        );
        assert!(f.state.lock().unwrap().comments.is_empty());

        // Neither Resume nor feedback may make room by closing an equivalent proposal.
        assert!(
            w.orchestrator
                .resume_pull_request(&tracked.id, "human")
                .unwrap_err()
                .to_string()
                .contains("existing adaptation proposals")
        );
        assert!(
            w.orchestrator
                .request_revision(&id, "Reassess", "human")
                .unwrap_err()
                .to_string()
                .contains("existing adaptation proposals")
        );
        assert!(
            w.orchestrator
                .revise_proposal(&id, "Revise", "human")
                .unwrap_err()
                .to_string()
                .contains("source PR changed")
        );
        assert!(f.state.lock().unwrap().comments.is_empty());

        let mut state = f.state.lock().unwrap();
        let proposal = state.proposals.get_mut(&published.number).unwrap();
        proposal["state"] = json!("closed");
        proposal["merged"] = json!(true);
        drop(state);
        assert_eq!(w.orchestrator.reconcile_task(&id).unwrap(), TaskState::Done);
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].status,
            "This head has already been evaluated"
        );
        for _ in 0..2 {
            reopen(&mut w, &f);
            assert!(scan(&mut w).is_empty());
            let report = w.orchestrator.tick().unwrap();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert!(report.discovered.is_empty());
        }
        assert_eq!(task_state(&w, &id), TaskState::Done);
        assert_eq!(w.orchestrator.db.list_tasks(None).unwrap().len(), 1);
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
            1
        );
        source_untouched(&f);
    }
}

#[test]
fn human_merged_adaptation_cleans_up_its_branch_without_changing_delivery_success() {
    for case in ["unchanged", "absent", "moved"] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        {
            let mut state = f.state.lock().unwrap();
            let proposal = state.proposals.get_mut(&published.number).unwrap();
            proposal["state"] = json!("closed");
            proposal["merged"] = json!(true);
        }
        let repo = w.forge.ensure_repository("org/app").unwrap();
        let branch = format!("refs/heads/{}", published.source_branch);
        let moved = repo
            .find_commit(Oid::from_str(&published.head_commit).unwrap())
            .unwrap()
            .parent_id(0)
            .unwrap();
        if case == "absent" {
            repo.find_reference(&branch).unwrap().delete().unwrap();
        }
        if case == "moved" {
            repo.reference(&branch, moved, true, "human push after merge")
                .unwrap();
        }
        assert_eq!(w.orchestrator.reconcile_task(&id).unwrap(), TaskState::Done);
        if case == "moved" {
            assert_eq!(repo.refname_to_id(&branch).unwrap(), moved);
        } else {
            assert!(repo.find_reference(&branch).is_err());
        }
        let events = w.orchestrator.db.list_events(&id).unwrap();
        let planned = events
            .iter()
            .find(|e| e.kind == "merge_request.branch_delete_planned")
            .unwrap();
        let outcome = events
            .iter()
            .find(|e| {
                e.kind
                    == if case == "moved" {
                        "merge_request.branch_delete_failed"
                    } else {
                        "merge_request.branch_deleted"
                    }
            })
            .unwrap();
        assert!(planned.id < outcome.id);
        assert_eq!(outcome.payload["head"], published.head_commit);
        assert_eq!(outcome.payload["state"], "merged");
        reopen(&mut w, &f);
        assert_eq!(w.orchestrator.reconcile_task(&id).unwrap(), TaskState::Done);
        source_untouched(&f);
    }
}

#[test]
fn a_merged_adaptation_can_be_observed_together_with_its_equivalent_rebase() {
    for held in [false, true] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        rebase_source(&w, &f);
        if held {
            hold_as_an_earlier_release_did(&mut w, &id);
        }
        {
            let mut state = f.state.lock().unwrap();
            let proposal = state.proposals.get_mut(&published.number).unwrap();
            proposal["state"] = json!("closed");
            proposal["merged"] = json!(true);
        }
        assert!(scan(&mut w).is_empty());
        let report = w.orchestrator.tick().unwrap();
        assert!(report.errors.is_empty(), "held={held}: {:?}", report.errors);
        assert!(report.discovered.is_empty());
        assert_eq!(task_state(&w, &id), TaskState::Done);
        reopen(&mut w, &f);
        assert!(scan(&mut w).is_empty());
        let tracked = w
            .orchestrator
            .db
            .list_pull_request_sources(None)
            .unwrap()
            .remove(0);
        assert!(!tracked.paused);
        assert_eq!(tracked.runs, 1);
        assert_eq!(tracked.status, "This head has already been evaluated");
        assert!(f.state.lock().unwrap().comments.is_empty());
        source_untouched(&f);
    }
}

#[test]
fn a_changed_head_after_a_preserved_rebase_still_supersedes() {
    let (mut w, f) = publishing_world();
    let id = published_adaptation(&mut w);
    for observations in 1..=3 {
        rebase_source(&w, &f);
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::AwaitingReview
        );
        assert!(scan(&mut w).is_empty());
        assert_eq!(
            w.orchestrator
                .db
                .list_events(&id)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "source.equivalent_head_observed")
                .count(),
            observations
        );
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
            1
        );
        reopen(&mut w, &f);
    }
    move_source_head(&w, &f);
    assert_eq!(
        w.orchestrator.reconcile_task(&id).unwrap(),
        TaskState::Closed
    );
    assert_eq!(f.state.lock().unwrap().comments.len(), 1);
    assert_eq!(scan(&mut w).len(), 1);
    assert_eq!(
        w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
        2
    );
}

#[test]
fn unavailable_or_ambiguous_equivalence_evidence_keeps_supersession() {
    for case in [
        "fetch",
        "raced_ref",
        "empty_change",
        "unrelated_history",
        "ambiguous_merge_bases",
        "missing_object",
    ] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let task = w.orchestrator.db.get_task(&id).unwrap();
        let frozen = artidezi_orchestrator::discovery::source_of(&task)
            .unwrap()
            .unwrap();
        let (target, _) = rebase_source(&w, &f);
        let repo = w.forge.ensure_repository("org/app").unwrap();
        match case {
            "fetch" => repo
                .find_reference("refs/pull/33/head")
                .unwrap()
                .delete()
                .unwrap(),
            "raced_ref" => {
                repo.reference(
                    "refs/pull/33/head",
                    Oid::from_str(target.as_str()).unwrap(),
                    true,
                    "race",
                )
                .unwrap();
            }
            "empty_change" => {
                repo.reference(
                    "refs/pull/33/head",
                    Oid::from_str(target.as_str()).unwrap(),
                    true,
                    "empty",
                )
                .unwrap();
                f.state.lock().unwrap().head = target.0;
            }
            "unrelated_history" => {
                let tree = repo
                    .find_commit(Oid::from_str(frozen.input.commit.as_str()).unwrap())
                    .unwrap()
                    .tree()
                    .unwrap();
                let sig = git2::Signature::now("Fixture", "fixture@invalid").unwrap();
                let head = repo
                    .commit(None, &sig, &sig, "Unrelated root", &tree, &[])
                    .unwrap();
                repo.reference("refs/pull/33/head", head, true, "unrelated")
                    .unwrap();
                f.state.lock().unwrap().head = head.to_string();
            }
            "ambiguous_merge_bases" => {
                let first = repo
                    .find_commit(Oid::from_str(frozen.input.commit.as_str()).unwrap())
                    .unwrap();
                let second = repo
                    .find_commit(Oid::from_str(target.as_str()).unwrap())
                    .unwrap();
                let tree = repo
                    .find_reference("refs/pull/33/head")
                    .unwrap()
                    .peel_to_tree()
                    .unwrap();
                let sig = git2::Signature::now("Fixture", "fixture@invalid").unwrap();
                let target = repo
                    .commit(None, &sig, &sig, "First merge", &tree, &[&first, &second])
                    .unwrap();
                let head = repo
                    .commit(None, &sig, &sig, "Second merge", &tree, &[&second, &first])
                    .unwrap();
                assert_eq!(repo.merge_bases(target, head).unwrap().len(), 2);
                repo.reference("refs/heads/main", target, true, "ambiguous target")
                    .unwrap();
                repo.reference("refs/pull/33/head", head, true, "ambiguous source")
                    .unwrap();
                f.state.lock().unwrap().head = head.to_string();
            }
            "missing_object" => {
                // A rebuilt cache has the current remote history but not the old, now unreachable
                // source head. Missing frozen objects must not be confused with equivalence.
                let cache = w.orchestrator.cache("github", "org/app").unwrap();
                std::fs::remove_dir_all(cache.path()).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::Closed,
            "{case}"
        );
        let events = w.orchestrator.db.list_events(&id).unwrap();
        assert!(
            events
                .iter()
                .all(|e| e.kind != "source.equivalent_head_observed"),
            "{case}"
        );
        assert!(
            events.iter().any(|e| e.kind == "assessment.superseded"
                && e.payload["expected_head"] == frozen.input.commit.0),
            "{case}"
        );
        assert_eq!(f.state.lock().unwrap().comments.len(), 1, "{case}");
        source_untouched(&f);
    }
}

#[test]
fn completed_reviews_and_dry_runs_do_not_reuse_equivalent_heads() {
    for publishing in [false, true] {
        let (mut w, f) = if publishing {
            publishing_world()
        } else {
            world()
        };
        let id = scan(&mut w).remove(0);
        assert_eq!(w.work(&unchanged(&w)), AttemptState::Valid);
        assert_eq!(task_state(&w, &id), TaskState::Done);
        rebase_source(&w, &f);
        assert_eq!(scan(&mut w).len(), 1);
        assert!(
            w.orchestrator
                .db
                .list_merge_requests(&id)
                .unwrap()
                .is_empty()
        );
        assert!(
            w.orchestrator
                .db
                .list_events(&id)
                .unwrap()
                .iter()
                .all(|e| e.kind != "source.equivalent_head_observed")
        );
        assert_eq!(
            w.orchestrator.db.list_pull_request_sources(None).unwrap()[0].runs,
            2
        );
    }
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
        assert!(
            w.forge
                .ensure_repository("org/app")
                .unwrap()
                .find_reference(&format!("refs/heads/{}", published.source_branch))
                .is_err()
        );
        let events = w.orchestrator.db.list_events(&id).unwrap();
        let planned = events
            .iter()
            .find(|e| e.kind == "merge_request.branch_delete_planned")
            .unwrap();
        let deleted = events
            .iter()
            .find(|e| e.kind == "merge_request.branch_deleted")
            .unwrap();
        assert!(planned.id < deleted.id);
        assert_eq!(planned.payload, deleted.payload);
        assert_eq!(deleted.payload["head"], published.head_commit);
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
fn superseded_branch_cleanup_refuses_a_moved_branch_but_accepts_an_absent_one() {
    for absent in [false, true] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        let new_head = move_source_head(&w, &f);
        let repo = w.forge.ensure_repository("org/app").unwrap();
        let branch = format!("refs/heads/{}", published.source_branch);
        if absent {
            repo.find_reference(&branch).unwrap().delete().unwrap();
        } else {
            // Git moves independently of the API's last observation. The close still sees the
            // recorded head, but receive-pack must reject the cleanup's original lease.
            repo.reference(
                &branch,
                Oid::from_str(new_head.as_str()).unwrap(),
                true,
                "human push",
            )
            .unwrap();
        }
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::Closed
        );
        let events = w.orchestrator.db.list_events(&id).unwrap();
        let kind = if absent {
            "merge_request.branch_deleted"
        } else {
            "merge_request.branch_delete_failed"
        };
        let cleanup = events.iter().find(|event| event.kind == kind).unwrap();
        assert_eq!(cleanup.payload["head"], published.head_commit);
        if absent {
            assert!(repo.find_reference(&branch).is_err());
        } else {
            assert_eq!(repo.refname_to_id(&branch).unwrap().to_string(), new_head.0);
            assert!(
                cleanup.payload["reason"]
                    .as_str()
                    .unwrap()
                    .contains("push lease failed")
            );
        }
        reopen(&mut w, &f);
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::Closed
        );
        assert_eq!(
            w.orchestrator
                .db
                .list_events(&id)
                .unwrap()
                .iter()
                .filter(|e| e.kind == kind)
                .count(),
            1
        );
        source_untouched(&f);
    }
}

#[test]
fn branch_cleanup_recovers_close_and_delete_crash_boundaries_without_repeating_terminal_outcomes() {
    for boundary in [
        "closed",
        "planned",
        "deleted",
        "receipted",
        "failed",
        "skipped",
    ] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        move_source_head(&w, &f);
        let attempt = w.orchestrator.db.latest_attempt(&id).unwrap().unwrap();
        w.orchestrator
            .db
            .record_event(
                Some(&id),
                Some(&attempt.id),
                "orchestrator",
                "merge_request.close_planned",
                json!({"repository":published.repository_id,"number":published.number}),
            )
            .unwrap();
        // Simulate loss of the close response, before updating the local receipt.
        f.state
            .lock()
            .unwrap()
            .proposals
            .get_mut(&published.number)
            .unwrap()["state"] = json!("closed");
        let payload = json!({"repository":published.repository_id,"forge":published.forge,"remote":"org/app",
            "number":published.number,"branch":published.source_branch,"head":published.head_commit});
        if boundary != "closed" {
            w.orchestrator
                .db
                .record_event(
                    Some(&id),
                    Some(&attempt.id),
                    "orchestrator",
                    "merge_request.branch_delete_planned",
                    payload.clone(),
                )
                .unwrap();
        }
        let repo = w.forge.ensure_repository("org/app").unwrap();
        let branch = format!("refs/heads/{}", published.source_branch);
        if boundary == "deleted" {
            repo.find_reference(&branch).unwrap().delete().unwrap();
        }
        let terminal = match boundary {
            "receipted" => Some("merge_request.branch_deleted"),
            "failed" => Some("merge_request.branch_delete_failed"),
            "skipped" => Some("merge_request.branch_delete_skipped"),
            _ => None,
        };
        if let Some(kind) = terminal {
            w.orchestrator
                .db
                .record_event(Some(&id), Some(&attempt.id), "orchestrator", kind, payload)
                .unwrap();
            // Even a recreated branch at the SAME head is protected by a terminal receipt.
            repo.find_reference(&branch).unwrap().delete().unwrap();
            repo.reference(
                &branch,
                Oid::from_str(&published.head_commit).unwrap(),
                false,
                "recreated",
            )
            .unwrap();
        }
        reopen(&mut w, &f);
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::Closed,
            "{boundary}"
        );
        assert_eq!(
            repo.find_reference(&branch).is_ok(),
            terminal.is_some(),
            "{boundary}"
        );
        let events = w.orchestrator.db.list_events(&id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "merge_request.branch_delete_planned")
                .count(),
            1,
            "{boundary}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == terminal.unwrap_or("merge_request.branch_deleted"))
                .count(),
            1,
            "{boundary}"
        );
    }
}

#[test]
fn open_proposals_and_unreceipted_publication_plans_protect_a_superseded_branch() {
    for case in ["open_proposal", "pending_plan"] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        let new_head = move_source_head(&w, &f);
        if case == "pending_plan" {
            // A plan for another head of the same branch may stand for a push whose receipt was lost.
            w.orchestrator.db.record_event(Some(&id), None, "orchestrator", "publication.planned",
                json!({"repository":published.repository_id,"branch":published.source_branch,"head":new_head})).unwrap();
        } else {
            let mut state = f.state.lock().unwrap();
            let mut other = state.proposals[&published.number].clone();
            other["number"] = json!(published.number + 1);
            state.proposals.insert(published.number + 1, other);
        }
        assert_eq!(
            w.orchestrator.reconcile_task(&id).unwrap(),
            TaskState::Closed
        );
        let repo = w.forge.ensure_repository("org/app").unwrap();
        assert_eq!(
            repo.refname_to_id(&format!("refs/heads/{}", published.source_branch))
                .unwrap()
                .to_string(),
            published.head_commit
        );
        let events = w.orchestrator.db.list_events(&id).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == "merge_request.branch_delete_skipped")
        );
        assert!(
            !events
                .iter()
                .any(|e| e.kind == "merge_request.branch_delete_planned")
        );
    }
}

#[test]
fn current_changed_or_paused_adaptations_are_never_closed_automatically() {
    for case in ["current", "pushed", "foreign", "paused", "read_only"] {
        let (mut w, f) = publishing_world();
        let id = published_adaptation(&mut w);
        let published = w
            .orchestrator
            .db
            .list_merge_requests(&id)
            .unwrap()
            .remove(0);
        match case {
            "read_only" => {
                w.orchestrator
                    .config
                    .projects
                    .get_mut("roles")
                    .unwrap()
                    .repositories
                    .get_mut("role")
                    .unwrap()
                    .agent_access = artidezi_contract::common::RepositoryAccess::ReadOnly;
            }
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
