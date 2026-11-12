// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use artidezi_contract::common::CommitSha;
use artidezi_forge::{MergeMethod, NewMergeRequest};
use chrono::Utc;
use serde_json::json;

fn published(w: &mut World) -> (Task, PublishedMergeRequest) {
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: maintain the library.",
            "operator",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("cleanup.sh", &proposal_script(false));
    assert_eq!(w.work(&script), AttemptState::Valid);
    let mr = w
        .orchestrator
        .db
        .list_merge_requests(&task.id)
        .unwrap()
        .remove(0);
    w.forge
        .merge(
            "org/role",
            mr.number,
            &CommitSha(mr.head_commit.clone()),
            MergeMethod::Merge,
        )
        .unwrap();
    (task, mr)
}

fn reopen(w: &mut World) {
    let config = w.orchestrator.config.clone();
    let forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::from([(
        "fake".into(),
        Box::new(w.forge.clone()) as Box<dyn ForgeAdapter>,
    )]);
    w.orchestrator = Orchestrator::open(config, &w.root.join("data"), forges).unwrap();
}

fn branch(mr: &PublishedMergeRequest) -> String {
    format!("refs/heads/{}", mr.source_branch)
}

fn kinds(w: &World, task: &Task, kind: &str) -> usize {
    w.orchestrator
        .db
        .list_events(&task.id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind)
        .count()
}

#[test]
fn manual_human_merge_cleanup_accepts_absent_branches_and_refuses_moved_ones() {
    for case in [
        "unchanged",
        "absent",
        "moved",
        "open_proposal",
        "pending_publication",
        "read_only",
    ] {
        let mut w = World::new();
        let (task, mr) = published(&mut w);
        let repo = w.forge.ensure_repository("org/role").unwrap();
        let original = Oid::from_str(&mr.head_commit).unwrap();
        let moved = repo.find_commit(original).unwrap().parent_id(0).unwrap();
        match case {
            "absent" => repo.find_reference(&branch(&mr)).unwrap().delete().unwrap(),
            "moved" => {
                repo.reference(&branch(&mr), moved, true, "human push after merge")
                    .unwrap();
            }
            "open_proposal" => {
                w.forge
                    .open_merge_request_as(
                        "org/role",
                        "human",
                        &NewMergeRequest {
                            title: "Keep this branch".into(),
                            body: String::new(),
                            source_branch: mr.source_branch.clone(),
                            target_branch: "main".into(),
                            draft: false,
                        },
                    )
                    .unwrap();
            }
            "pending_publication" => {
                w.orchestrator
                    .db
                    .record_event(
                        Some(&task.id),
                        None,
                        "orchestrator",
                        "manual_publication.planned",
                        json!({"repository":mr.repository_id,"head":moved.to_string()}),
                    )
                    .unwrap();
            }
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
            _ => {}
        }
        assert_eq!(
            w.orchestrator.reconcile_task(&task.id).unwrap(),
            TaskState::Done,
            "{case}"
        );
        if ["unchanged", "absent"].contains(&case) {
            assert!(repo.find_reference(&branch(&mr)).is_err());
            assert_eq!(kinds(&w, &task, "merge_request.branch_deleted"), 1);
        } else {
            assert_eq!(
                repo.refname_to_id(&branch(&mr)).unwrap(),
                if case == "moved" { moved } else { original }
            );
            let kind = if case == "moved" {
                "merge_request.branch_delete_failed"
            } else {
                "merge_request.branch_delete_skipped"
            };
            assert_eq!(kinds(&w, &task, kind), 1, "{case}");
        }
        // Terminal outcomes protect even a recreated branch at the original head.
        repo.reference(&branch(&mr), original, true, "recreated")
            .unwrap();
        reopen(&mut w);
        w.orchestrator
            .tick_at(Utc::now() + Duration::minutes(5))
            .unwrap();
        assert_eq!(
            repo.refname_to_id(&branch(&mr)).unwrap(),
            original,
            "{case}"
        );
        assert!(
            w.orchestrator
                .db
                .due_branch_cleanups(Utc::now() + Duration::hours(1), 16)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn terminal_merge_cleanup_retries_transient_git_failure_and_stops_at_its_budget() {
    for recover in [true, false] {
        let mut w = World::new();
        let (task, mr) = published(&mut w);
        let repo = w.forge.ensure_repository("org/role").unwrap();
        let lock = repo.path().join(format!("{}.lock", branch(&mr)));
        fs::write(&lock, "held by another git operation").unwrap();
        assert_eq!(
            w.orchestrator.reconcile_task(&task.id).unwrap(),
            TaskState::Done
        );
        assert_eq!(kinds(&w, &task, "merge_request.branch_delete_planned"), 1);
        assert_eq!(
            kinds(&w, &task, "merge_request.branch_delete_retry_scheduled"),
            1
        );
        let later = Utc::now() + Duration::minutes(2);
        assert!(
            w.orchestrator
                .db
                .due_branch_cleanups(Utc::now(), 16)
                .unwrap()
                .is_empty()
        );
        if recover {
            fs::remove_file(&lock).unwrap();
        }
        reopen(&mut w);
        w.orchestrator.tick_at(later).unwrap();
        w.orchestrator
            .tick_at(later + Duration::minutes(2))
            .unwrap();
        assert_eq!(
            w.orchestrator.db.get_task(&task.id).unwrap().state,
            TaskState::Done
        );
        assert_eq!(repo.find_reference(&branch(&mr)).is_err(), recover);
        assert_eq!(
            kinds(&w, &task, "merge_request.branch_deleted"),
            usize::from(recover)
        );
        assert_eq!(
            kinds(&w, &task, "merge_request.branch_delete_failed"),
            usize::from(!recover)
        );
        assert_eq!(kinds(&w, &task, "merge_request.branch_delete_planned"), 1);
        if !recover {
            fs::remove_file(&lock).unwrap();
        }
        w.orchestrator.tick_at(later + Duration::hours(1)).unwrap();
        assert_eq!(repo.find_reference(&branch(&mr)).is_err(), recover);
        assert!(
            w.orchestrator
                .db
                .due_branch_cleanups(later + Duration::hours(1), 16)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn merged_cleanup_recovers_crash_boundaries_after_done_with_the_original_lease() {
    for boundary in [
        "receipt",
        "planned",
        "deleted",
        "terminal",
        "moved",
        "exhausted",
    ] {
        let mut w = World::new();
        let (task, mut mr) = published(&mut w);
        mr.state = MergeRequestState::Merged;
        // The merged receipt and queue commit together, before the task becomes terminal.
        w.orchestrator.db.upsert_merge_request(&mr).unwrap();
        w.orchestrator
            .db
            .set_task_state(&task.id, TaskState::Done, "forge")
            .unwrap();
        let payload = json!({"repository":mr.repository_id,"forge":mr.forge,"remote":"org/role",
            "number":mr.number,"branch":mr.source_branch,"head":mr.head_commit,"state":"merged"});
        let now = Utc::now();
        if boundary != "receipt" {
            w.orchestrator
                .db
                .begin_branch_cleanup(&mr, now)
                .unwrap()
                .unwrap();
            w.orchestrator
                .db
                .record_event(
                    Some(&task.id),
                    None,
                    "orchestrator",
                    "merge_request.branch_delete_planned",
                    payload.clone(),
                )
                .unwrap();
        }
        if boundary == "exhausted" {
            for minutes in [2, 4] {
                w.orchestrator
                    .db
                    .begin_branch_cleanup(&mr, now + Duration::minutes(minutes))
                    .unwrap()
                    .unwrap();
            }
        }
        let repo = w.forge.ensure_repository("org/role").unwrap();
        if ["deleted", "terminal"].contains(&boundary) {
            repo.find_reference(&branch(&mr)).unwrap().delete().unwrap();
        }
        if boundary == "terminal" {
            w.orchestrator
                .db
                .record_event(
                    Some(&task.id),
                    None,
                    "orchestrator",
                    "merge_request.branch_deleted",
                    payload,
                )
                .unwrap();
            repo.reference(
                &branch(&mr),
                Oid::from_str(&mr.head_commit).unwrap(),
                false,
                "recreated",
            )
            .unwrap();
        }
        if boundary == "moved" {
            let moved = repo
                .find_commit(Oid::from_str(&mr.head_commit).unwrap())
                .unwrap()
                .parent_id(0)
                .unwrap();
            repo.reference(&branch(&mr), moved, true, "moved after crash")
                .unwrap();
            // A later receipt must never replace the queue's lease.
            let mut changed = mr.clone();
            changed.head_commit = moved.to_string();
            w.orchestrator.db.upsert_merge_request(&changed).unwrap();
        }
        reopen(&mut w);
        w.orchestrator.tick_at(now + Duration::minutes(6)).unwrap();
        let kept = ["terminal", "moved", "exhausted"].contains(&boundary);
        assert_eq!(
            repo.find_reference(&branch(&mr)).is_ok(),
            kept,
            "{boundary}"
        );
        assert_eq!(kinds(&w, &task, "merge_request.branch_delete_planned"), 1);
        assert_eq!(
            kinds(&w, &task, "merge_request.branch_deleted"),
            usize::from(!["moved", "exhausted"].contains(&boundary))
        );
        assert!(
            w.orchestrator
                .db
                .due_branch_cleanups(now + Duration::hours(1), 16)
                .unwrap()
                .is_empty()
        );
    }
}
