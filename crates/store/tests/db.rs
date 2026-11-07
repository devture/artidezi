// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use artidezi_config::model::DeliveryMode;
use artidezi_contract::common::{ArtifactRef, Outcome};
use artidezi_store::model::*;
use artidezi_store::{Db, StoreError, new_id};
use chrono::{Duration, Utc};

fn artifact(c: char) -> ArtifactRef {
    ArtifactRef {
        sha256: std::iter::repeat_n(c, 64).collect(),
        size: 10,
    }
}

#[test]
fn worker_drain_cooldown_and_bounded_recovery_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite");
    let mut db = Db::open(&path).unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let first = attempt(&t.id, 1);
    db.insert_attempt(&first).unwrap();
    let now = Utc::now();
    db.lease_next_attempt("personal", "failed-worker", Duration::seconds(120), now)
        .unwrap()
        .unwrap();
    db.drain_worker("personal", "failed-worker", true, "operator")
        .unwrap();
    // Drain does not revoke current work or its heartbeats.
    db.heartbeat(&first.id, "failed-worker", Duration::seconds(120), now)
        .unwrap();
    let second_task = task();
    db.insert_task(&second_task).unwrap();
    db.insert_attempt(&attempt(&second_task.id, 1)).unwrap();
    assert!(
        db.lease_next_attempt("personal", "failed-worker", Duration::seconds(120), now)
            .unwrap()
            .is_none()
    );
    let expired = now + Duration::seconds(121);
    assert_eq!(
        db.expire_leases(expired).unwrap(),
        std::slice::from_ref(&first.id)
    );
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(
        db.pending_lost_attempts().unwrap(),
        [db.get_attempt(&first.id).unwrap()]
    );
    let retry = attempt(&t.id, 2);
    assert!(db.recover_lost_attempt(&first.id, Some(&retry), 1).unwrap());
    assert!(!db.recover_lost_attempt(&first.id, Some(&retry), 1).unwrap());
    db.drain_worker("personal", "failed-worker", false, "operator")
        .unwrap();
    db.observe_worker("personal", "failed-worker", Some(false), expired)
        .unwrap();
    assert!(
        db.lease_next_attempt("personal", "failed-worker", Duration::seconds(120), expired)
            .unwrap()
            .is_none()
    );
    let other = db
        .lease_next_attempt(
            "personal",
            "healthy-worker",
            Duration::seconds(120),
            expired,
        )
        .unwrap()
        .unwrap();
    // The healthy worker may finish unrelated queued work before the retry.
    db.cancel_attempt(&other.0.id, "operator").unwrap();
    let leased_retry = if other.0.id == retry.id {
        panic!("older unrelated work should be selected first");
    } else {
        db.lease_next_attempt(
            "personal",
            "healthy-worker",
            Duration::seconds(120),
            expired,
        )
        .unwrap()
        .unwrap()
    };
    assert_eq!(leased_retry.0.id, retry.id);
    assert!(
        db.lease_next_attempt(
            "personal",
            "healthy-worker",
            Duration::seconds(120),
            expired
        )
        .unwrap()
        .is_none()
    );
    db.expire_leases(expired + Duration::seconds(121)).unwrap();
    let third = attempt(&t.id, 3);
    assert!(!db.recover_lost_attempt(&retry.id, Some(&third), 1).unwrap());
    assert_eq!(db.get_task(&t.id).unwrap().state, TaskState::NeedsHuman);
    assert_eq!(db.list_attempts(&t.id).unwrap().len(), 2);
    assert!(
        db.heartbeat(&first.id, "failed-worker", Duration::seconds(120), expired)
            .is_err()
    );
    let workers = db.list_workers().unwrap();
    assert_eq!(workers.len(), 2);
    assert!(workers.iter().all(|w| w.cooldown_until.is_some()));
}

#[test]
fn forgetting_a_worker_leaves_its_lease_to_expire_and_recover() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let first = attempt(&t.id, 1);
    db.insert_attempt(&first).unwrap();
    let now = Utc::now();
    db.observe_worker("personal", "removed", None, now).unwrap();
    db.observe_worker("personal", "kept", None, now).unwrap();
    db.lease_next_attempt("personal", "removed", Duration::seconds(120), now)
        .unwrap()
        .unwrap();

    db.forget_unlisted_workers(&[("personal", "kept")]).unwrap();
    let workers = db.list_workers().unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].worker_id, "kept");

    let lost = db.expire_leases(now + Duration::seconds(121)).unwrap();
    assert_eq!(lost, vec![first.id.clone()]);
    let retry = attempt(&t.id, 2);
    assert!(db.recover_lost_attempt(&first.id, Some(&retry), 1).unwrap());
    assert_eq!(db.list_workers().unwrap().len(), 1);
}

#[test]
fn recovery_never_reopens_closed_tasks_or_reruns_publication_history() {
    for closed in [false, true] {
        let mut db = Db::open_in_memory().unwrap();
        let t = task();
        db.insert_task(&t).unwrap();
        let a = attempt(&t.id, 1);
        db.insert_attempt(&a).unwrap();
        let now = Utc::now();
        db.lease_next_attempt("personal", "worker", Duration::seconds(120), now)
            .unwrap();
        db.expire_leases(now + Duration::seconds(121)).unwrap();
        if closed {
            db.set_task_state(&t.id, TaskState::Closed, "operator")
                .unwrap();
        } else {
            db.record_event(
                Some(&t.id),
                Some(&a.id),
                "operator",
                "publication.planned",
                serde_json::json!({}),
            )
            .unwrap();
        }
        assert!(
            !db.recover_lost_attempt(&a.id, Some(&attempt(&t.id, 2)), 2)
                .unwrap()
        );
        assert_eq!(db.list_attempts(&t.id).unwrap().len(), 1);
        assert_eq!(
            db.get_task(&t.id).unwrap().state,
            if closed {
                TaskState::Closed
            } else {
                TaskState::NeedsHuman
            }
        );
    }
}

#[test]
fn a_prior_accepted_record_blocks_automatic_reexecution_of_a_lost_revision() {
    let mut db = Db::open_in_memory().unwrap();
    let task = task();
    db.insert_task(&task).unwrap();
    let first = attempt(&task.id, 1);
    db.insert_attempt(&first).unwrap();
    let now = Utc::now();
    db.lease_next_attempt("personal", "worker-1", Duration::seconds(120), now)
        .unwrap();
    db.record_upload(&first.id, "worker-1", &artifact('b'), None, None)
        .unwrap();
    db.judge_attempt(&first.id, vec![]).unwrap();
    let revision = attempt(&task.id, 2);
    db.insert_attempt(&revision).unwrap();
    db.lease_next_attempt("personal", "worker-2", Duration::seconds(120), now)
        .unwrap();
    db.expire_leases(now + Duration::seconds(121)).unwrap();
    assert!(
        !db.recover_lost_attempt(&revision.id, Some(&attempt(&task.id, 3)), 2)
            .unwrap()
    );
    assert_eq!(db.get_task(&task.id).unwrap().state, TaskState::NeedsHuman);
    assert_eq!(db.list_attempts(&task.id).unwrap().len(), 2);
}

fn task() -> Task {
    let now = Utc::now();
    Task {
        id: new_id("tsk"),
        number: 0,
        project: "acme-roles".into(),
        kind: TaskKind::Manual,
        created_at: now,
        updated_at: now,
        created_by: "slavi".into(),
        instructions: "bump it".into(),
        delivery_mode: DeliveryMode::MergeRequest,
        agent_profile: "codex-personal".into(),
        policy_snapshot: "{}".into(),
        policy_digest: "0".repeat(64),
        state: TaskState::Working,
        repositories: vec![TaskRepository {
            repository_id: "ansible-role-notes".into(),
            branch: "main".into(),
            base_commit: "0".repeat(40),
        }],
    }
}

fn attempt(task_id: &str, number: u32) -> Attempt {
    let now = Utc::now();
    Attempt {
        id: new_id("att"),
        task_id: task_id.into(),
        number,
        state: AttemptState::Queued,
        created_at: now,
        updated_at: now,
        worker_pool: "personal".into(),
        job: artifact('a'),
        human_feedback: None,
        not_before: None,
        attempt_record: None,
        result: None,
        outcome: None,
        validation_problems: vec![],
    }
}

#[test]
fn discovery_queue_and_source_claim_are_atomic() {
    let mut db = Db::open_in_memory().unwrap();
    let mut t = task();
    t.kind = TaskKind::PullRequest;
    t.delivery_mode = DeliveryMode::Shadow;
    let source = TrackedPullRequest {
        id: new_id("pr"),
        project: t.project.clone(),
        repository_id: "role".into(),
        forge: "fake".into(),
        remote: "org/role".into(),
        number: 1,
        url: "http://fake.invalid/1".into(),
        title: "Update library".into(),
        closed: false,
        handler_id: "renovate".into(),
        observed_head: "a".repeat(40),
        head_observed_at: Utc::now(),
        last_task_id: Some(t.id.clone()),
        runs: 1,
        paused: false,
        status: "queued".into(),
        follow_up: None,
    };
    let mut old_record = serde_json::to_value(&source).unwrap();
    old_record.as_object_mut().unwrap().remove("follow_up");
    let restored: TrackedPullRequest = serde_json::from_value(old_record).unwrap();
    assert_eq!(restored, source);
    let invalid = attempt("missing-task", 1);
    assert!(db.insert_discovery_task(&t, &invalid, &source).is_err());
    assert!(db.list_tasks(None).unwrap().is_empty());
    assert!(db.list_pull_request_sources(None).unwrap().is_empty());
    db.insert_discovery_task(&t, &attempt(&t.id, 1), &source)
        .unwrap();
    assert_eq!(db.list_tasks(None).unwrap().len(), 1);
    assert_eq!(db.get_pull_request_source(&source.id).unwrap(), source);
    let mut scheduled = source.clone();
    scheduled.follow_up = Some(PullRequestFollowUp {
        at: Utc::now() + Duration::minutes(2),
        reason: PullRequestFollowUpReason::Checks,
    });
    db.save_pull_request_source(&scheduled).unwrap();
    assert_eq!(db.get_pull_request_source(&source.id).unwrap(), scheduled);
    assert!(
        db.lease_next_attempt("personal", "worker", Duration::minutes(1), Utc::now())
            .unwrap()
            .is_some()
    );
}

#[test]
fn ci_wait_and_poll_deadline_survive_restart_and_stop_on_state_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let mut db = Db::open(&path).unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let now = Utc::now();
    let due = now + Duration::minutes(10);
    db.wait_for_source_ci(&t.id, due).unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.get_task(&t.id).unwrap().state, TaskState::WaitingForCi);
    assert!(!db.claim_source_ci_check(&t.id, now, due).unwrap());
    let next = due + Duration::minutes(10);
    assert!(db.claim_source_ci_check(&t.id, due, next).unwrap());
    assert!(!db.clone().claim_source_ci_check(&t.id, due, next).unwrap());
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert!(!db.claim_source_ci_check(&t.id, due, next).unwrap());
    db.set_task_state(&t.id, TaskState::Closed, "human")
        .unwrap();
    assert!(
        !db.claim_source_ci_check(&t.id, next, next + Duration::minutes(10))
            .unwrap()
    );
    assert!(db.wait_for_source_ci(&t.id, next).is_err());
}

#[test]
fn task_round_trip_and_transitions() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let loaded = db.get_task(&t.id).unwrap();
    assert_eq!(loaded.repositories, t.repositories);
    assert_eq!(loaded.state, TaskState::Working);

    db.set_task_state(&t.id, TaskState::AwaitingReview, "orchestrator")
        .unwrap();
    db.set_task_state(&t.id, TaskState::Done, "slavi").unwrap();
    assert!(matches!(
        db.set_task_state(&t.id, TaskState::Working, "slavi"),
        Err(StoreError::InvalidTransition { .. })
    ));
    assert_eq!(db.list_tasks(Some(TaskState::Done)).unwrap().len(), 1);
    assert_eq!(db.list_tasks(Some(TaskState::Working)).unwrap().len(), 0);

    let events = db.list_events(&t.id).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].kind, "task.created");
    assert_eq!(events[2].payload["to"], "done");
}

#[test]
fn lease_lifecycle() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let a = attempt(&t.id, 1);
    db.insert_attempt(&a).unwrap();

    let now = Utc::now();
    assert!(
        db.lease_next_attempt("other-pool", "w1", Duration::minutes(5), now)
            .unwrap()
            .is_none()
    );
    let (leased, lease) = db
        .lease_next_attempt("personal", "w1", Duration::minutes(5), now)
        .unwrap()
        .unwrap();
    assert_eq!(leased.id, a.id);
    assert_eq!(lease.worker_id, "w1");
    assert!(
        db.lease_next_attempt("personal", "w2", Duration::minutes(5), now)
            .unwrap()
            .is_none()
    );

    // Another worker cannot heartbeat or upload.
    assert!(
        db.heartbeat(&a.id, "w2", Duration::minutes(5), now)
            .is_err()
    );
    assert!(
        db.record_upload(&a.id, "w2", &artifact('b'), None, None)
            .is_err()
    );

    let lease = db
        .heartbeat(&a.id, "w1", Duration::minutes(10), now)
        .unwrap();
    assert_eq!(lease.expires_at, now + Duration::minutes(10));

    db.record_upload(
        &a.id,
        "w1",
        &artifact('b'),
        Some(&artifact('c')),
        Some(Outcome::Proposal),
    )
    .unwrap();
    let uploaded = db.get_attempt(&a.id).unwrap();
    assert_eq!(uploaded.state, AttemptState::Uploaded);
    assert_eq!(uploaded.outcome, Some(Outcome::Proposal));
    assert_eq!(uploaded.result, Some(artifact('c')));
    assert!(db.get_lease(&a.id).unwrap().is_none());

    assert_eq!(
        db.judge_attempt(&a.id, vec!["bad".into()]).unwrap(),
        AttemptState::Invalid
    );
    assert_eq!(
        db.get_attempt(&a.id).unwrap().validation_problems,
        vec!["bad".to_string()]
    );
    assert!(db.judge_attempt(&a.id, vec![]).is_err());
}

#[test]
fn heartbeats_cannot_revive_expired_leases_or_cross_a_fixed_deadline() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    let a = attempt(&t.id, 1);
    db.insert_task(&t).unwrap();
    db.insert_attempt(&a).unwrap();
    let start = Utc::now();
    db.lease_next_attempt("personal", "w1", Duration::minutes(5), start)
        .unwrap()
        .unwrap();
    let deadline = start + Duration::minutes(8);
    let lease = db
        .heartbeat_until(
            &a.id,
            "w1",
            Duration::minutes(10),
            start + Duration::minutes(4),
            deadline,
        )
        .unwrap();
    assert_eq!(lease.expires_at, deadline);
    assert!(
        db.heartbeat_until(&a.id, "w1", Duration::minutes(10), deadline, deadline)
            .is_err()
    );
    assert!(
        db.heartbeat(&a.id, "w1", Duration::minutes(10), deadline)
            .is_err()
    );
    assert_eq!(
        db.expire_leases(deadline + Duration::seconds(1)).unwrap(),
        std::slice::from_ref(&a.id)
    );
    assert_eq!(db.get_attempt(&a.id).unwrap().state, AttemptState::Lost);
}

#[test]
fn overdue_pre_upgrade_lease_is_shortened_only_by_its_owner() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    let a = attempt(&t.id, 1);
    db.insert_task(&t).unwrap();
    db.insert_attempt(&a).unwrap();
    let start = Utc::now();
    let original = start + Duration::hours(24);
    db.lease_next_attempt("personal", "w1", Duration::hours(24), start)
        .unwrap()
        .unwrap();
    let deadline = start + Duration::hours(2);
    let now = start + Duration::hours(3);
    assert!(
        db.heartbeat_until(&a.id, "w2", Duration::minutes(15), now, deadline)
            .is_err()
    );
    assert_eq!(
        db.get_lease(&a.id)
            .unwrap()
            .unwrap()
            .expires_at
            .timestamp_micros(),
        original.timestamp_micros()
    );
    assert!(
        db.heartbeat_until(&a.id, "w1", Duration::minutes(15), now, deadline)
            .is_err()
    );
    assert_eq!(db.expire_leases(now).unwrap(), [a.id]);
}

#[test]
fn expired_leases_become_lost() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let a = attempt(&t.id, 1);
    db.insert_attempt(&a).unwrap();
    let now = Utc::now();
    db.lease_next_attempt("personal", "w1", Duration::minutes(5), now)
        .unwrap()
        .unwrap();
    assert!(
        db.expire_leases(now + Duration::minutes(4))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.expire_leases(now + Duration::minutes(6)).unwrap(),
        vec![a.id.clone()]
    );
    assert_eq!(db.get_attempt(&a.id).unwrap().state, AttemptState::Lost);
    assert_eq!(db.latest_attempt(&t.id).unwrap().unwrap().number, 1);
}

#[test]
fn merge_requests_upsert() {
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let mut mr = PublishedMergeRequest {
        task_id: t.id.clone(),
        repository_id: "ansible-role-notes".into(),
        forge: "github-acme".into(),
        number: 74,
        url: "https://example.invalid/pr/74".into(),
        source_branch: "artidezi/tsk_x".into(),
        head_commit: "1".repeat(40),
        state: MergeRequestState::Open,
        updated_at: Utc::now(),
    };
    db.upsert_merge_request(&mr).unwrap();
    mr.head_commit = "2".repeat(40);
    mr.state = MergeRequestState::Merged;
    db.upsert_merge_request(&mr).unwrap();
    let loaded = db
        .get_merge_request(&t.id, "ansible-role-notes")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.head_commit, "2".repeat(40));
    assert_eq!(loaded.state, MergeRequestState::Merged);
    assert_eq!(db.list_merge_requests(&t.id).unwrap().len(), 1);
}

#[test]
fn database_file_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artidezi.sqlite");
    let t = task();
    {
        let mut db = Db::open(&path).unwrap();
        db.insert_task(&t).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.get_task(&t.id).unwrap().id, t.id);
}

#[test]
fn task_numbers_backfill_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgrade.sqlite");
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy
        .execute_batch(include_str!("../src/migrations/0001_initial.sql"))
        .unwrap();
    legacy
        .execute_batch(include_str!("../src/migrations/0002_pr_discovery.sql"))
        .unwrap();
    legacy.pragma_update(None, "user_version", 2).unwrap();
    for (id, date) in [("old-b", "2026-01-02"), ("old-a", "2026-01-01")] {
        legacy.execute(
            "INSERT INTO tasks VALUES (?1, 'project', 'manual', ?2, ?2, 'admin', 'Existing task', 'merge_request', 'fake', '{}', '', 'working')",
            rusqlite::params![id, format!("{date}T00:00:00Z")],
        ).unwrap();
    }
    drop(legacy);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.get_task_by_number(1).unwrap().id, "old-a");
    assert_eq!(db.get_task("old-b").unwrap().number, 2);
    let new = task();
    db.insert_task(&new).unwrap();
    assert_eq!(db.get_task(&new.id).unwrap().number, 3);
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.get_task_by_number(3).unwrap().id, new.id);
    let next = task();
    db.insert_task(&next).unwrap();
    assert_eq!(db.get_task(&next.id).unwrap().number, 4);
    assert!(db.get_task_by_number(999).is_err());
}

#[test]
fn cloned_handles_lease_once_and_share_cancellation() {
    use std::sync::{Arc, Barrier};
    let mut db = Db::open_in_memory().unwrap();
    let t = task();
    let a = attempt(&t.id, 1);
    db.insert_task(&t).unwrap();
    db.insert_attempt(&a).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let winners = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let mut handle = db.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    handle
                        .lease_next_attempt(
                            "personal",
                            &format!("worker-{i}"),
                            Duration::minutes(1),
                            Utc::now(),
                        )
                        .unwrap()
                })
            })
            .collect();
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(winners.len(), 1);
    let mut transport = db.clone();
    transport
        .heartbeat(
            &a.id,
            &winners[0].1.worker_id,
            Duration::minutes(1),
            Utc::now(),
        )
        .unwrap();
    assert!(db.get_lease(&a.id).unwrap().is_some());
    db.cancel_attempt(&a.id, "admin").unwrap();
    assert!(
        transport
            .heartbeat(
                &a.id,
                &winners[0].1.worker_id,
                Duration::minutes(1),
                Utc::now()
            )
            .is_err()
    );
    assert!(transport.get_lease(&a.id).unwrap().is_none());
    assert_eq!(
        transport.get_attempt(&a.id).unwrap().state,
        AttemptState::Cancelled
    );
}

#[test]
fn pausing_queued_work_races_safely_with_leasing() {
    use std::sync::{Arc, Barrier};
    for _ in 0..16 {
        let mut db = Db::open_in_memory().unwrap();
        let t = task();
        let a = attempt(&t.id, 1);
        db.insert_task(&t).unwrap();
        db.insert_attempt(&a).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let mut transport = db.clone();
        let mut control = db.clone();
        let gate = barrier.clone();
        let (leased, paused) = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                gate.wait();
                transport
                    .lease_next_attempt("personal", "worker", Duration::minutes(1), Utc::now())
                    .unwrap()
            });
            let pause = scope.spawn(|| {
                barrier.wait();
                control.cancel_queued_attempt(&a.id, "admin").unwrap()
            });
            (worker.join().unwrap(), pause.join().unwrap())
        });
        assert_eq!(paused, leased.is_none());
        assert_eq!(
            db.get_attempt(&a.id).unwrap().state,
            if paused {
                AttemptState::Cancelled
            } else {
                AttemptState::Leased
            }
        );
        assert_eq!(db.get_lease(&a.id).unwrap().is_none(), paused);
    }
}

#[test]
fn transient_retry_backoff_budget_and_maintenance_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite");
    let mut db = Db::open(&path).unwrap();
    let t = task();
    db.insert_task(&t).unwrap();
    let now = Utc::now();
    db.set_discovery_paused(true, "operator").unwrap();
    for number in 1..=3 {
        let current = if number == 1 {
            let a = attempt(&t.id, 1);
            db.insert_attempt(&a).unwrap();
            a
        } else {
            db.latest_attempt(&t.id).unwrap().unwrap()
        };
        let run_at = current.not_before.unwrap_or(now);
        if number > 1 {
            assert!(
                db.lease_next_attempt(
                    "personal",
                    "worker",
                    Duration::seconds(120),
                    run_at - Duration::milliseconds(1)
                )
                .unwrap()
                .is_none()
            );
        }
        db.lease_next_attempt("personal", "worker", Duration::seconds(120), run_at)
            .unwrap()
            .unwrap();
        db.record_upload(&current.id, "worker", &artifact('b'), None, None)
            .unwrap();
        db.judge_attempt(&current.id, vec!["no result document".into()])
            .unwrap();
        db.record_transient_failure(&current.id, "model_capacity")
            .unwrap();
        let next = attempt(&t.id, number + 1);
        let replaced = db
            .recover_transient_attempt(&current.id, &next, 2, run_at)
            .unwrap();
        assert_eq!(replaced, number <= 2);
        drop(db);
        db = Db::open(&path).unwrap();
        assert!(db.discovery_paused().unwrap());
    }
    assert_eq!(db.get_task(&t.id).unwrap().state, TaskState::NeedsHuman);
    assert_eq!(db.list_attempts(&t.id).unwrap().len(), 3);
    db.set_discovery_paused(false, "operator").unwrap();
    assert!(!db.discovery_paused().unwrap());
}

#[test]
fn a_database_from_a_newer_version_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("newer.sqlite");
    drop(Db::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert!(version > 0);
    connection
        .pragma_update(None, "user_version", version + 1)
        .unwrap();
    drop(connection);

    assert!(matches!(
        Db::open(&path),
        Err(artidezi_store::StoreError::SchemaTooNew { .. })
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    let after: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(after, version + 1);
}
