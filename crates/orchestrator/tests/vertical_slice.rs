// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The offline vertical slice: configuration → task → job → in-process worker with the scripted
//! fake harness → ingestion and validation → publication to the fake forge → revision → automerge
//! and direct push. No model, no network, no real forge.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::TaskSelection;
use artidezi_forge::fake::FakeForge;
use artidezi_forge::{ChecksState, ForgeAdapter};
use artidezi_harness::fake::FakeHarness;
use artidezi_orchestrator::Orchestrator;
use artidezi_store::model::*;
use artidezi_worker::{AttemptInputs, PreviousInputs, WorkerConfig, run_attempt};
use chrono::Duration;
use git2::{Oid, Repository};

#[path = "support/discovery.rs"]
mod discovery;
#[path = "support/github.rs"]
mod github;

#[test]
fn lost_execution_reuses_frozen_inputs_and_recovers_without_forge_access() {
    let mut w = World::new();
    w.orchestrator
        .config
        .worker_pools
        .get_mut("personal")
        .unwrap()
        .lost_attempt_retries = 2;
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: preserve these instructions.",
            "operator",
            &TaskSelection::default(),
        )
        .unwrap();
    let original = w
        .orchestrator
        .lease("personal", "lost-worker", Duration::seconds(120))
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now() + Duration::seconds(121);
    let mut access = w.orchestrator.state_access();
    assert_eq!(
        access.recover_expired_at(now).unwrap(),
        std::slice::from_ref(&original.attempt.id)
    );
    assert!(access.recover_expired_at(now).unwrap().is_empty());
    let next = access.db.latest_attempt(&task.id).unwrap().unwrap();
    assert_eq!(next.number, 2);
    assert_eq!(next.state, AttemptState::Queued);
    let mut expected = original.job.clone();
    expected.attempt_id = next.id.as_str().into();
    expected.attempt_number = 2;
    assert_eq!(access.load_job(&next).unwrap(), expected);
    assert!(
        access
            .require_live_lease(&access.db.get_attempt(&original.attempt.id).unwrap(), now)
            .is_err()
    );
    assert_eq!(
        access.db.get_task(&task.id).unwrap().state,
        TaskState::Working
    );
    let script = w.script("recovered.sh", &proposal_script(false));
    let state = w.work(&script);
    assert_eq!(
        state,
        AttemptState::Valid,
        "{:?}",
        w.orchestrator
            .db
            .latest_attempt(&task.id)
            .unwrap()
            .unwrap()
            .validation_problems
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::AwaitingReview
    );
    assert_eq!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .len(),
        1
    );
}

const CONFIG: &str = r#"
schema_version: 1
instance:
  name: test
  bot_identity: {name: ArtiDezi, email: bot@artidezi.invalid}
  data_dir: /nonexistent-overridden-in-test
  admin_token_ref: admin-token
model_endpoints:
  fake: {provider: none, auth_mode: none, credential_ref: none}
worker_pools:
  personal: {harness_sandbox: disabled, harnesses: [fake], workers: {w1: {token_ref: worker-token}, w2: {token_ref: worker-2-token}}}
agent_profiles:
  fake: {harness: fake, harness_version_constraint: "*", model: none, model_endpoint: fake, worker_pool: personal}
forges:
  fake: {provider: fake, api_base_url: http://fake.invalid, credential_ref: none, bot_login: "artidezi[bot]"}
projects:
  roles:
    description: A role and the playbook that consumes it.
    agent_instructions: Preserve the current interface across the configured repositories.
    default_agent_profile: fake
    allowed_agent_profiles: [fake]
    default_delivery_mode: merge_request
    allowed_delivery_modes: [merge_request, merge_request_with_automerge, direct_push_fast_forward]
    draft_merge_requests: false
    network:
      command_egress: {mode: blocked, denied_domains: [], private_network: blocked, allowed_private_endpoints: []}
      web_search: {mode: disabled, domain_scope: public_internet}
    limits: {wall_clock_seconds: 60, result_json_max_bytes: 65536, max_commits_per_repository: 5, max_diff_bytes_per_repository: 65536, max_commit_message_bytes: 1024}
    effects: {merge_renovate_merge_request_on_safe_verdict: false, publish_adaptation_merge_requests: true}
    repositories:
      role:
        forge: fake
        remote: org/role
        branch: main
        description: The role.
        agent_access: read_write
        verification_recipe:
          - {name: changes-exist, command: [test, -f, CHANGES.md], timeout_seconds: 10}
        tagging: {agent_may_propose: true, allowed_pattern: 'v[0-9]+\.[0-9]+\.[0-9]+-[0-9]+'}
      playbook:
        forge: fake
        remote: org/playbook
        branch: main
        description: Read-only context.
        agent_access: read_only
"#;

fn seed(repo: &Repository, parent: Option<Oid>, file: &str, content: &str, msg: &str) -> Oid {
    let mut builder = match parent {
        Some(p) => repo
            .treebuilder(Some(&repo.find_commit(p).unwrap().tree().unwrap()))
            .unwrap(),
        None => repo.treebuilder(None).unwrap(),
    };
    let blob = repo.blob(content.as_bytes()).unwrap();
    builder.insert(file, blob, 0o100644).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("Upstream", "up@invalid").unwrap();
    let parents: Vec<git2::Commit> = parent
        .map(|p| repo.find_commit(p).unwrap())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        msg,
        &tree,
        &parent_refs,
    )
    .unwrap()
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    orchestrator: Orchestrator,
    forge: FakeForge,
}

impl World {
    fn new() -> Self {
        Self::with_config(CONFIG)
    }

    fn with_config(source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let forge_root = root.join("forge");
        let forge = FakeForge::new(&forge_root);
        let role = forge.ensure_repository("org/role").unwrap();
        let c1 = seed(&role, None, "README.md", "role\n", "Initial");
        seed(
            &role,
            Some(c1),
            "defaults.yml",
            "version: 1.0.0\n",
            "Add defaults",
        );
        let playbook = forge.ensure_repository("org/playbook").unwrap();
        seed(&playbook, None, "README.md", "playbook\n", "Initial");

        let config = artidezi_config::parse(source).unwrap();
        let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
        forges.insert("fake".into(), Box::new(forge.clone()));
        let orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();
        Self {
            _dir: dir,
            root,
            orchestrator,
            forge,
        }
    }

    fn forge(&self) -> FakeForge {
        self.forge.clone()
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -e\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Leases the next attempt, runs it through the in-process worker and ingests the result.
    fn work(&mut self, script: &Path) -> AttemptState {
        self.work_with_record(script, |_| {})
    }

    fn work_with_record(
        &mut self,
        script: &Path,
        modify: impl FnOnce(&mut artidezi_contract::attempt::AttemptRecord),
    ) -> AttemptState {
        let leased = self
            .orchestrator
            .lease("personal", "w1", Duration::minutes(5))
            .unwrap()
            .expect("queued attempt");
        let previous = leased.previous.as_ref().map(|p| PreviousInputs {
            attempt_document: p.attempt_document.clone(),
            result_document: p.result_document.clone(),
            commit_bundles: p.commit_bundles.clone(),
        });
        let inputs = AttemptInputs {
            job: &leased.job,
            job_bytes: &leased.job_bytes,
            task_document: &leased.task_document,
            snapshot_bundles: leased.snapshot_bundles.clone(),
            input_ref_bundles: leased.input_ref_bundles.clone(),
            previous,
        };
        let config = WorkerConfig {
            worker_pool: "personal".into(),
            worker_id: "w1".into(),
            workspaces_root: self.root.join("workspaces"),
            credentials_root: None,
            max_log_bytes: 65536,
        };
        let harness = FakeHarness {
            script: script.to_path_buf(),
        };
        let output = run_attempt(&config, &harness, &inputs).unwrap();
        let record_bytes = fs::read(output.workspace.join("out/attempt.json")).unwrap();
        let mut record = serde_json::from_slice(&record_bytes).unwrap();
        modify(&mut record);
        let record_bytes = serde_json::to_vec(&record).unwrap();
        let files: Vec<(String, PathBuf)> = output
            .artifacts
            .iter()
            .map(|a| (a.name.clone(), a.path.clone()))
            .collect();
        let state = self
            .orchestrator
            .ingest(&leased.attempt.id, "w1", &record_bytes, &files)
            .unwrap();
        output.cleanup().unwrap();
        state
    }
}

fn retention_policy() -> artidezi_config::model::ArtifactRetention {
    artidezi_config::model::ArtifactRetention {
        completed_task_bundle_retention_seconds: 86400,
        orphan_grace_seconds: 86400,
        cleanup_interval_seconds: 3600,
    }
}

#[test]
fn heartbeating_worker_cannot_hold_an_attempt_beyond_its_frozen_execution_budget() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Inspect the library update",
            "test",
            &TaskSelection::default(),
        )
        .unwrap();
    let work = w
        .orchestrator
        .lease("personal", "worker", Duration::minutes(5))
        .unwrap()
        .unwrap();
    let lease = w
        .orchestrator
        .db
        .get_lease(&work.attempt.id)
        .unwrap()
        .unwrap();
    // 60s harness + 10s verification + 15s termination grace per command + 30m overhead.
    let deadline = lease.leased_at + Duration::seconds(1900);
    // A later configuration change cannot extend this already-frozen attempt.
    w.orchestrator
        .config
        .projects
        .get_mut("roles")
        .unwrap()
        .limits
        .wall_clock_seconds = 86400;
    for seconds in (60..1900).step_by(60) {
        w.orchestrator
            .state_access()
            .heartbeat_at(
                &work.attempt.id,
                "worker",
                Duration::minutes(5),
                lease.leased_at + Duration::seconds(seconds),
            )
            .unwrap();
    }
    assert_eq!(
        w.orchestrator
            .db
            .get_lease(&work.attempt.id)
            .unwrap()
            .unwrap()
            .expires_at,
        deadline
    );
    assert!(
        w.orchestrator
            .state_access()
            .heartbeat_at(&work.attempt.id, "worker", Duration::minutes(5), deadline,)
            .is_err()
    );
    assert!(
        w.orchestrator
            .state_access()
            .require_live_lease(&work.attempt, deadline)
            .is_err()
    );
    let report = w
        .orchestrator
        .tick_at(deadline + Duration::seconds(1))
        .unwrap();
    assert_eq!(report.lost_attempts, [work.attempt.id]);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
}

#[test]
fn retention_preserves_shared_live_inputs_then_expires_closed_task_inputs_idempotently() {
    let mut w = World::new();
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    let closed = w
        .orchestrator
        .create_manual_task("roles", "First task", "test", &TaskSelection::default())
        .unwrap();
    let active = w
        .orchestrator
        .create_manual_task("roles", "Second task", "test", &TaskSelection::default())
        .unwrap();
    let attempt = w
        .orchestrator
        .db
        .latest_attempt(&closed.id)
        .unwrap()
        .unwrap();
    let job: artidezi_contract::Job = w.orchestrator.artifacts.get_json(&attempt.job).unwrap();
    let active_attempt = w
        .orchestrator
        .db
        .latest_attempt(&active.id)
        .unwrap()
        .unwrap();
    let active_job: artidezi_contract::Job = w
        .orchestrator
        .artifacts
        .get_json(&active_attempt.job)
        .unwrap();
    assert_eq!(
        job.repositories[0].snapshot_bundle,
        active_job.repositories[0].snapshot_bundle
    );
    w.orchestrator.close_task(&closed.id, "test").unwrap();
    let later = chrono::Utc::now() + Duration::days(2);
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(later, true)
            .unwrap()
            .removed_bytes,
        0
    );
    assert!(
        w.orchestrator
            .artifacts
            .contains(&job.repositories[0].snapshot_bundle)
    );
    w.orchestrator.close_task(&active.id, "test").unwrap();
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(chrono::Utc::now(), false)
            .unwrap()
            .reclaimable_bytes,
        0
    );
    let preview = w.orchestrator.cleanup_artifacts_at(later, false).unwrap();
    assert!(preview.expired_bundle_files >= 2);
    assert!(
        w.orchestrator
            .artifacts
            .contains(&job.repositories[0].snapshot_bundle)
    );
    let applied = w.orchestrator.cleanup_artifacts_at(later, true).unwrap();
    assert_eq!(preview.reclaimable_bytes, applied.removed_bytes);
    assert!(
        !w.orchestrator
            .artifacts
            .contains(&job.repositories[0].snapshot_bundle)
    );
    assert!(
        w.orchestrator
            .db
            .artifact_was_deleted(&job.repositories[0].snapshot_bundle.sha256)
            .unwrap()
    );
    assert_eq!(
        w.orchestrator
            .artifacts
            .get_json::<artidezi_contract::Job>(&attempt.job)
            .unwrap(),
        job
    );
    assert!(w.orchestrator.artifacts.contains(&job.task_document));
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(later, true)
            .unwrap()
            .removed_bytes,
        0
    );
    // Extending retention cannot resurrect a terminal task's expired inputs.
    w.orchestrator
        .config
        .instance
        .artifact_retention
        .as_mut()
        .unwrap()
        .completed_task_bundle_retention_seconds = 86400 * 30;
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(chrono::Utc::now(), true)
            .unwrap()
            .removed_bytes,
        0
    );
    // Fresh tasks fetch new snapshots even after an identical old bundle was expired.
    let new_task = w
        .orchestrator
        .create_manual_task("roles", "After expiry", "test", &TaskSelection::default())
        .unwrap();
    assert!(
        w.orchestrator
            .lease("personal", "worker", Duration::minutes(5))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        w.orchestrator.db.get_task(&new_task.id).unwrap().state,
        TaskState::Working
    );
    assert_eq!(
        w.orchestrator
            .cleanup_artifacts_at(later, true)
            .unwrap()
            .removed_bytes,
        0
    );
}

#[test]
fn retention_keeps_open_proposals_and_unpublished_worker_evidence() {
    let mut w = World::new();
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: make a proposal.",
            "test",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(true));
    assert_eq!(w.work(&script), AttemptState::Valid);
    w.orchestrator.close_task(&task.id, "test").unwrap();
    let later = chrono::Utc::now() + Duration::days(2);
    let old_api = w.orchestrator.config.forges["fake"].api_base_url.clone();
    w.orchestrator
        .config
        .forges
        .get_mut("fake")
        .unwrap()
        .api_base_url = "http://changed.invalid".into();
    let failed_refresh = w.orchestrator.cleanup_artifacts_at(later, true).unwrap();
    assert!(failed_refresh.eligible_tasks.is_empty());
    assert_eq!(failed_refresh.publication_refresh_errors.len(), 1);
    w.orchestrator
        .config
        .forges
        .get_mut("fake")
        .unwrap()
        .api_base_url = old_api;
    assert!(
        w.orchestrator
            .cleanup_artifacts_at(later, true)
            .unwrap()
            .eligible_tasks
            .is_empty()
    );
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
            &mr.head_commit.as_str().into(),
            artidezi_forge::MergeMethod::Merge,
        )
        .unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let record_ref = attempt.attempt_record.unwrap();
    let record: artidezi_contract::attempt::AttemptRecord =
        w.orchestrator.artifacts.get_json(&record_ref).unwrap();
    let report = w.orchestrator.cleanup_artifacts_at(later, true).unwrap();
    assert!(report.removed_bytes > 0);
    assert!(report.publication_refresh_errors.is_empty());
    assert_eq!(
        Repository::open_bare(w.forge.repo_path("org/role"))
            .unwrap()
            .tag_names(None)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::Closed
    );
    assert_eq!(
        w.orchestrator.db.list_merge_requests(&task.id).unwrap()[0].state,
        MergeRequestState::Merged
    );
    assert!(w.orchestrator.artifacts.contains(&record_ref));
    assert!(w.orchestrator.artifacts.contains(&attempt.result.unwrap()));
    for repo in record.repositories {
        if let Some(series) = repo.series {
            assert!(w.orchestrator.artifacts.contains(&series.bundle.unwrap()));
            assert!(w.orchestrator.artifacts.contains(&series.diff.unwrap()));
        }
    }
    assert!(
        w.orchestrator
            .artifacts
            .contains(&record.harness.stdout.unwrap())
    );
}

#[test]
fn retention_protects_inflight_and_recent_deduplicated_uploads_and_fails_closed() {
    let mut w = World::new();
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    let task = w
        .orchestrator
        .create_manual_task("roles", "Upload test", "test", &TaskSelection::default())
        .unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let upload = w.orchestrator.artifacts.put(b"unaccepted upload").unwrap();
    let orphan = w.orchestrator.artifacts.put(b"unused file").unwrap();
    let recent = chrono::Utc::now() + Duration::days(2);
    w.orchestrator
        .db
        .pin_artifact_upload(&attempt.id, &upload, recent)
        .unwrap();
    let preview = w.orchestrator.cleanup_artifacts_at(recent, false).unwrap();
    assert_eq!(preview.orphan_files, 1);
    assert_eq!(preview.candidates[0].artifact, orphan);
    w.orchestrator.close_task(&task.id, "test").unwrap();
    // The old file's mtime is insufficient: a fresh upload of identical bytes must protect it.
    w.orchestrator.cleanup_artifacts_at(recent, true).unwrap();
    assert!(w.orchestrator.artifacts.contains(&upload));
    assert!(!w.orchestrator.artifacts.contains(&orphan));
    w.orchestrator
        .cleanup_artifacts_at(recent + Duration::days(2), true)
        .unwrap();
    assert!(!w.orchestrator.artifacts.contains(&upload));
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Missing retained input",
            "test",
            &TaskSelection::default(),
        )
        .unwrap();
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let job: artidezi_contract::Job = w.orchestrator.artifacts.get_json(&attempt.job).unwrap();
    let extra = w
        .orchestrator
        .artifacts
        .put(b"old orphan must survive failed preflight")
        .unwrap();
    fs::remove_file(
        w.orchestrator
            .artifacts
            .path(&job.repositories[0].snapshot_bundle)
            .unwrap(),
    )
    .unwrap();
    assert!(
        w.orchestrator
            .cleanup_artifacts_at(recent + Duration::days(4), true)
            .is_err()
    );
    assert!(w.orchestrator.artifacts.contains(&extra));
}

#[test]
fn verification_records_must_match_every_configured_step() {
    let config = CONFIG.replace(
        "- {name: changes-exist, command: [test, -f, CHANGES.md], timeout_seconds: 10}",
        "- {name: changes-exist, command: [test, -f, CHANGES.md], timeout_seconds: 10}\n          - {name: changes-not-empty, command: [test, -s, CHANGES.md], timeout_seconds: 10}",
    );
    for case in ["valid", "missing", "duplicate", "command", "cwd", "extra"] {
        let mut w = World::with_config(&config);
        let task = w
            .orchestrator
            .create_manual_task(
                "roles",
                "Do the thing: record a change.",
                "slavi",
                &TaskSelection::default(),
            )
            .unwrap();
        let script = w.script("agent.sh", &proposal_script(false));
        let state = w.work_with_record(&script, |record| {
            let steps = &mut record
                .repositories
                .iter_mut()
                .find(|r| r.id.as_str() == "role")
                .unwrap()
                .verification;
            assert_eq!(steps.len(), 2);
            match case {
                "valid" => {}
                "missing" => {
                    steps.pop();
                }
                "duplicate" => steps[1] = steps[0].clone(),
                "command" => steps[0].command = vec!["true".into()],
                "cwd" => steps[0].cwd = "..".into(),
                "extra" => steps.push(steps[0].clone()),
                _ => unreachable!(),
            }
        });
        if case == "valid" {
            assert_eq!(state, AttemptState::Valid);
        } else {
            assert_eq!(state, AttemptState::Invalid, "{case}");
            assert_eq!(
                w.orchestrator.db.get_task(&task.id).unwrap().state,
                TaskState::NeedsHuman
            );
            assert!(
                w.orchestrator
                    .db
                    .list_merge_requests(&task.id)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

fn result_writer(json: &str) -> String {
    format!(
        "printf '%s' '{}' > \"$ARTIDEZI_RESULT_PATH\"",
        json.replace('\'', "'\\''")
    )
}

/// Appends a line to CHANGES.md, commits it and reports a proposal (with a unique tag proposal).
fn proposal_script(with_tag: bool) -> String {
    let tag = if with_tag {
        r#", "tag_proposal": "v1.0.0-TAGN""#
    } else {
        ""
    };
    let json = format!(
        r#"{{"schema_version": 1, "outcome": "proposal", "summary": "Recorded a change.",
 "risk_hint": {{"level": "low", "rationale": "trivial"}},
 "evidence": [{{"kind": "local_check", "source": "cat CHANGES.md", "note": "line appended"}}],
 "repositories": [{{"id": "role", "changed": true, "head_commit": "HEADSHA"{tag}}}]}}"#
    );
    format!(
        "grep -q 'Do the thing' \"$ARTIDEZI_TASK_DOCUMENT\"\n\
         echo \"change by $ARTIDEZI_ATTEMPT_ID\" >> CHANGES.md\n\
         git add CHANGES.md\n\
         git commit -q -m \"Record change for $ARTIDEZI_ATTEMPT_ID\"\n\
         HEAD=$(git rev-parse HEAD)\n\
         {}\n\
         sed -i \"s/HEADSHA/$HEAD/; s/TAGN/$$/\" \"$ARTIDEZI_RESULT_PATH\"\n",
        result_writer(&json)
    )
}

#[test]
fn manual_proposal_changing_protected_paths_waits_for_authorization() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = proposal_script(false).replace(
        "git add CHANGES.md",
        "mkdir -p .github/workflows\necho 'name: ci' > .github/workflows/ci.yml\ngit add CHANGES.md .github/workflows/ci.yml",
    );
    let script = w.script("agent.sh", &script);
    assert_eq!(w.work(&script), AttemptState::Valid);

    let held = w.orchestrator.db.get_task(&task.id).unwrap();
    assert_eq!(held.state, TaskState::NeedsHuman);
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );

    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let record = attempt.attempt_record.as_ref().unwrap();
    let task = w
        .orchestrator
        .authorize_workflow_publication(
            &task.id,
            &attempt.id,
            &record.sha256,
            "Reviewed the workflow change",
            "slavi",
        )
        .unwrap();

    assert_eq!(task.state, TaskState::AwaitingReview);
    assert_eq!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn attempt_left_uploaded_by_a_crash_is_rejected_on_the_next_tick() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &TaskSelection::default())
        .unwrap();
    let attempt = w
        .orchestrator
        .db
        .lease_next_attempt("personal", "w1", Duration::hours(1), chrono::Utc::now())
        .unwrap()
        .unwrap()
        .0;
    let record = w.orchestrator.artifacts.put(b"{}").unwrap();
    w.orchestrator
        .db
        .record_upload(&attempt.id, "w1", &record, None, None)
        .unwrap();

    w.orchestrator.tick().unwrap();

    let attempt = w.orchestrator.db.get_attempt(&attempt.id).unwrap();
    assert_eq!(attempt.state, AttemptState::Invalid);
    let task = w.orchestrator.db.get_task(&task.id).unwrap();
    assert_eq!(task.state, TaskState::NeedsHuman);
}

#[test]
fn reviewers_see_the_patch_built_from_the_git_objects() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(false));
    let forged = artidezi_contract::common::ArtifactRef {
        sha256: "0".repeat(64),
        size: 0,
    };
    let state = w.work_with_record(&script, |record| {
        for collection in &mut record.repositories {
            if let Some(series) = collection.series.as_mut().filter(|s| s.diff.is_some()) {
                series.diff = Some(forged.clone());
            }
        }
    });
    assert_eq!(state, AttemptState::Valid);

    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let record: artidezi_contract::attempt::AttemptRecord = w
        .orchestrator
        .artifacts
        .get_json(attempt.attempt_record.as_ref().unwrap())
        .unwrap();
    let diff = record
        .repositories
        .iter()
        .find_map(|c| c.series.as_ref()?.diff.as_ref())
        .unwrap();
    let patch = String::from_utf8(w.orchestrator.artifacts.get(diff).unwrap()).unwrap();
    assert!(patch.contains("+change by"), "{patch}");
}

#[test]
fn release_tag_points_at_the_merge_commit_after_the_target_moves() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(true));
    assert_eq!(w.work(&script), AttemptState::Valid);
    let mr = w
        .orchestrator
        .db
        .list_merge_requests(&task.id)
        .unwrap()
        .remove(0);
    let forge = w.forge();
    let info = forge.get_merge_request("org/role", mr.number).unwrap();
    let merged = forge
        .merge(
            "org/role",
            mr.number,
            &info.head_commit,
            artidezi_forge::MergeMethod::Merge,
        )
        .unwrap();
    let repo = Repository::open_bare(forge.repo_path("org/role")).unwrap();
    seed(
        &repo,
        Some(Oid::from_str(&merged.0).unwrap()),
        "later.txt",
        "later\n",
        "An unrelated later commit",
    );

    assert_eq!(
        w.orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::Done
    );

    let events = w.orchestrator.db.list_events(&task.id).unwrap();
    let tag = events.iter().find(|e| e.kind == "tag.created").unwrap();
    assert_eq!(tag.payload["commit"], merged.0.as_str());
}

#[test]
fn manual_publication_retries_after_a_push_without_a_merge_request() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    w.forge().fail_next_create("org/role");
    let script = w.script("agent.sh", &proposal_script(false));
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );

    let task = w.orchestrator.retry_publication(&task.id, "slavi").unwrap();

    assert_eq!(task.state, TaskState::AwaitingReview);
    let mrs = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(mrs.len(), 1);
}

#[test]
fn manual_task_interrupted_after_validation_needs_a_human_on_restart() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(false));
    assert_eq!(w.work(&script), AttemptState::Valid);
    w.orchestrator
        .db
        .set_task_state(&task.id, TaskState::Working, "test")
        .unwrap();

    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert("fake".into(), Box::new(w.forge()));
    let reopened =
        Orchestrator::open(w.orchestrator.config.clone(), &w.root.join("data"), forges).unwrap();

    assert_eq!(
        reopened.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
}

#[test]
fn merge_request_delivery_with_revision() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    assert_eq!(task.state, TaskState::Working);
    assert_eq!(task.repositories.len(), 2);
    assert_eq!(task.repositories[1].repository_id, "role");
    let attempts = w.orchestrator.db.list_attempts(&task.id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, AttemptState::Queued);

    let script = w.script("agent.sh", &proposal_script(false));
    assert_eq!(w.work(&script), AttemptState::Valid);
    let task = w.orchestrator.db.get_task(&task.id).unwrap();
    assert_eq!(task.state, TaskState::AwaitingReview);

    let mrs = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(mrs.len(), 1);
    let forge = w.forge();
    let info = forge.get_merge_request("org/role", mrs[0].number).unwrap();
    assert_eq!(info.author_login, "artidezi[bot]");
    assert_eq!(info.source_branch, format!("artidezi/{}", task.id));
    assert_eq!(info.target_branch, "main");
    assert!(
        info.title.starts_with("Record change for att_"),
        "{}",
        info.title
    );
    assert!(
        info.body.contains("Local verification:** passed"),
        "{}",
        info.body
    );
    assert!(info.body.contains("risk hint"), "{}", info.body);
    assert_eq!(info.head_commit.as_str(), mrs[0].head_commit);
    // The published commit carries the bot identity, not the worker's.
    let repo = Repository::open_bare(forge.repo_path("org/role")).unwrap();
    let head = repo
        .find_commit(Oid::from_str(&mrs[0].head_commit).unwrap())
        .unwrap();
    assert_eq!(head.author().name().unwrap(), "ArtiDezi");
    assert_eq!(head.author().email().unwrap(), "bot@artidezi.invalid");
    // main is untouched.
    assert_eq!(
        forge.branch_head("org/role", "main").unwrap().as_str(),
        task.repositories[1].base_commit
    );

    // Revision: fresh attempt, same merge request, replaced branch head.
    let attempt2 = w
        .orchestrator
        .request_revision(&task.id, "Please also mention the date.", "slavi")
        .unwrap();
    assert_eq!(attempt2.number, 2);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::Working
    );
    let job2 = w.orchestrator.load_job(&attempt2).unwrap();
    assert_eq!(job2.previous_attempt.as_ref().unwrap().attempt_number, 1);
    let prompt =
        String::from_utf8(w.orchestrator.artifacts.get(&job2.task_document).unwrap()).unwrap();
    assert!(prompt.contains("mention the date"), "{prompt}");
    assert!(prompt.contains("PREVIOUS attempt 1"), "{prompt}");
    assert!(
        prompt.contains(
            "## Project instructions (trusted configuration)\n\nPreserve the current interface"
        ),
        "{prompt}"
    );

    let script2 = w.script(
        "agent2.sh",
        &format!(
            "test -f \"$ARTIDEZI_WORKSPACE/task/previous/result.json\"\n\
             test -f \"$ARTIDEZI_WORKSPACE/task/previous/role/commits.bundle\"\n\
             {}",
            proposal_script(false)
        ),
    );
    assert_eq!(w.work(&script2), AttemptState::Valid);
    let mrs2 = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(mrs2.len(), 1);
    assert_eq!(mrs2[0].number, mrs[0].number);
    assert_ne!(mrs2[0].head_commit, mrs[0].head_commit);
    let info2 = forge.get_merge_request("org/role", mrs[0].number).unwrap();
    assert_eq!(info2.head_commit.as_str(), mrs2[0].head_commit);
    assert!(info2.body.contains("attempt 2"), "{}", info2.body);
    // The replacement is one commit on the original base, not stacked on attempt 1.
    let head2 = repo
        .find_commit(Oid::from_str(&mrs2[0].head_commit).unwrap())
        .unwrap();
    assert_eq!(
        head2.parent_id(0).unwrap().to_string(),
        task.repositories[1].base_commit
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::AwaitingReview
    );

    // A human merges on the forge; reconciliation notices and closes the task.
    forge
        .merge(
            "org/role",
            mrs[0].number,
            &info2.head_commit,
            artidezi_forge::MergeMethod::Merge,
        )
        .unwrap();
    assert_eq!(
        w.orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::Done
    );
    assert_eq!(
        forge.branch_head("org/role", "main").unwrap().as_str(),
        mrs2[0].head_commit
    );

    let kinds: Vec<String> = w
        .orchestrator
        .db
        .list_events(&task.id)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"merge_request.published".to_string()));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "merge_request.published")
            .count(),
        2
    );
}

#[test]
fn automerge_waits_for_green_checks_and_tags() {
    let mut w = World::new();
    let selection = TaskSelection {
        agent_profile: None,
        delivery_mode: Some(DeliveryMode::MergeRequestWithAutomerge),
    };
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &selection)
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(true));
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::AwaitingReview
    );

    // Checks pending: nothing happens.
    assert_eq!(
        w.orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::AwaitingReview
    );
    let forge = w.forge();
    let mr = &w.orchestrator.db.list_merge_requests(&task.id).unwrap()[0];
    assert_eq!(
        forge
            .get_merge_request("org/role", mr.number)
            .unwrap()
            .state,
        artidezi_forge::MergeRequestState::Open
    );

    forge.set_checks("org/role", &mr.source_branch, ChecksState::Success);
    assert_eq!(
        w.orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::Done
    );
    assert_eq!(
        forge.branch_head("org/role", "main").unwrap().as_str(),
        mr.head_commit
    );
    let repo = Repository::open_bare(forge.repo_path("org/role")).unwrap();
    let tags: Vec<String> = repo
        .references_glob("refs/tags/*")
        .unwrap()
        .map(|r| {
            r.unwrap()
                .name()
                .unwrap()
                .trim_start_matches("refs/tags/")
                .to_owned()
        })
        .collect();
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert!(tags[0].starts_with("v1.0.0-"));
    assert_eq!(
        repo.refname_to_id(&format!("refs/tags/{}", tags[0]))
            .unwrap()
            .to_string(),
        mr.head_commit
    );
}

#[test]
fn direct_push_lands_immediately_and_refuses_a_moved_target() {
    let mut w = World::new();
    let selection = TaskSelection {
        agent_profile: None,
        delivery_mode: Some(DeliveryMode::DirectPushFastForward),
    };
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &selection)
        .unwrap();
    let script = w.script("agent.sh", &proposal_script(true));
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::Done
    );
    let forge = w.forge();
    let head = forge.branch_head("org/role", "main").unwrap();
    assert_ne!(head.as_str(), task.repositories[1].base_commit);
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );

    // Second task: someone pushes to main while the agent works; the push is refused.
    let task2 = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &selection)
        .unwrap();
    let repo = Repository::open_bare(forge.repo_path("org/role")).unwrap();
    let current = Oid::from_str(head.as_str()).unwrap();
    seed(
        &repo,
        Some(current),
        "OTHER.md",
        "someone else\n",
        "Concurrent change",
    );
    assert_eq!(w.work(&script), AttemptState::Valid);
    let task2 = w.orchestrator.db.get_task(&task2.id).unwrap();
    assert_eq!(task2.state, TaskState::NeedsHuman);
    let reasons: Vec<String> = w
        .orchestrator
        .db
        .list_events(&task2.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "task.needs_human")
        .map(|e| e.payload["reason"].as_str().unwrap().to_owned())
        .collect();
    assert!(reasons[0].contains("target branch moved"), "{reasons:?}");
}

#[test]
fn invalid_attempts_route_to_a_human() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &TaskSelection::default())
        .unwrap();

    // Dirty working tree after committing: series invalid.
    let script = w.script(
        "dirty.sh",
        &format!("{}\necho leftover >> README.md\n", proposal_script(false)),
    );
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let task = w.orchestrator.db.get_task(&task.id).unwrap();
    assert_eq!(task.state, TaskState::NeedsHuman);
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    assert!(
        attempt
            .validation_problems
            .iter()
            .any(|p| p.contains("series invalid")),
        "{:?}",
        attempt.validation_problems
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );

    // Revision after a human looked: the agent asks a question instead.
    w.orchestrator
        .request_revision(&task.id, "Try again, cleanly.", "slavi")
        .unwrap();
    let json = r#"{"schema_version": 1, "outcome": "needs_human", "summary": "Unsure.", "questions_for_human": ["Which version?"],
 "repositories": [{"id": "role", "changed": false}]}"#;
    let script = w.script("ask.sh", &result_writer(json));
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );

    // A harness that modifies the read-only repository is rejected.
    w.orchestrator
        .request_revision(&task.id, "Once more.", "slavi")
        .unwrap();
    let script = w.script(
        "tamper.sh",
        &format!(
            "cd \"$ARTIDEZI_WORKSPACE/repos/playbook\"\necho x >> README.md\ngit commit -q -am tamper\ncd - >/dev/null\n{}",
            proposal_script(false)
        ),
    );
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    assert!(
        attempt
            .validation_problems
            .iter()
            .any(|p| p.contains("read_only")),
        "{:?}",
        attempt.validation_problems
    );
}

#[test]
fn revision_after_lost_worker_keeps_feedback_without_previous_artifacts() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Do the thing: Record a maintenance note.",
            "admin",
            &TaskSelection::default(),
        )
        .unwrap();
    let leased = w
        .orchestrator
        .lease("personal", "lost-worker", Duration::minutes(5))
        .unwrap()
        .unwrap();
    w.orchestrator
        .db
        .expire_leases(chrono::Utc::now() + Duration::minutes(10))
        .unwrap();
    assert_eq!(
        w.orchestrator
            .db
            .latest_attempt(&task.id)
            .unwrap()
            .unwrap()
            .state,
        AttemptState::Lost
    );
    let revision = w
        .orchestrator
        .request_revision(&task.id, "Preserve the current interface.", "admin")
        .unwrap();
    let job = w.orchestrator.load_job(&revision).unwrap();
    assert!(job.previous_attempt.is_none());
    let document =
        String::from_utf8(w.orchestrator.artifacts.get(&job.task_document).unwrap()).unwrap();
    assert!(document.contains("Record a maintenance note."));
    assert!(document.contains("Preserve the current interface."));
    assert!(document.contains("<<<FEEDBACK_fence_"));
    assert!(!document.contains("<<<FEEDBACK\n"));
    assert!(document.contains("PREVIOUS attempt 1"));
    assert!(document.contains("No execution record or commits were returned"));
    assert_ne!(leased.attempt.id, revision.id);
    let script = w.script(
        "retry.sh",
        &format!(
            "test ! -d \"$ARTIDEZI_WORKSPACE/task/previous\"\n\
             grep -q 'Preserve the current interface.' \"$ARTIDEZI_WORKSPACE/task/task.md\"\n{}",
            proposal_script(false)
        ),
    );
    let state = w.work(&script);
    let latest = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    assert_eq!(
        state,
        AttemptState::Valid,
        "{:?}",
        latest.validation_problems
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::AwaitingReview
    );
}

#[test]
fn failing_verification_invalidates_the_attempt() {
    let mut w = World::new();
    let task = w
        .orchestrator
        .create_manual_task("roles", "Do the thing.", "slavi", &TaskSelection::default())
        .unwrap();
    // The recipe requires CHANGES.md; this agent commits a different file.
    let script = w.script(
        "wrong-file.sh",
        &proposal_script(false).replace("CHANGES.md", "OTHER.md"),
    );
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    assert!(
        attempt
            .validation_problems
            .iter()
            .any(|p| p.contains("verification step changes-exist failed")),
        "{:?}",
        attempt.validation_problems
    );
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        w.forge()
            .list_open_merge_requests("org/role", None)
            .unwrap()
            .len(),
        0
    );
}

fn two_repository_world() -> World {
    World::with_config(&CONFIG.replace(
        "[merge_request, merge_request_with_automerge, direct_push_fast_forward]",
        "[merge_request]",
    ).replace(
        "description: Read-only context.\n        agent_access: read_only",
        "description: The playbook consuming the role.\n        agent_access: read_write\n        verification_recipe:\n          - {name: role-and-playbook-agree, command: [cmp, API_VERSION, ../role/API_VERSION], timeout_seconds: 10}",
    ))
}

fn two_repository_script(role_version: &str, playbook_version: &str) -> String {
    let json = r#"{"schema_version":1,"outcome":"proposal","summary":"Upgrade the role and adapt its consumer.","repositories":[{"id":"role","changed":true,"head_commit":"ROLEHEAD"},{"id":"playbook","changed":true,"head_commit":"PLAYBOOKHEAD"}]}"#;
    format!(
        r#"
for repo in role playbook; do
    cd "$ARTIDEZI_WORKSPACE/repos/$repo"
    case "$repo" in role) version='{role_version}' ;; playbook) version='{playbook_version}' ;; esac
    printf '%s\n' "$version" > API_VERSION
    printf 'API version %s\n' "$version" > CHANGES.md
    git add API_VERSION CHANGES.md
    git commit -q -m "Adapt $repo to API $version"
done
role_head=$(git -C "$ARTIDEZI_WORKSPACE/repos/role" rev-parse HEAD)
playbook_head=$(git -C "$ARTIDEZI_WORKSPACE/repos/playbook" rev-parse HEAD)
{result}
sed -i "s/ROLEHEAD/$role_head/; s/PLAYBOOKHEAD/$playbook_head/" "$ARTIDEZI_RESULT_PATH"
"#,
        result = result_writer(json)
    )
}

#[test]
fn two_repositories_publish_revise_and_finish_only_after_both_merges() {
    let mut w = two_repository_world();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Upgrade both repositories.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let first = w.script("first.sh", &two_repository_script("2", "2"));
    assert_eq!(w.work(&first), AttemptState::Valid);
    let initial = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(initial.len(), 2);
    w.orchestrator
        .request_revision(&task.id, "Use API version 3 instead.", "slavi")
        .unwrap();
    let second = w.script("second.sh", &two_repository_script("3", "3"));
    assert_eq!(w.work(&second), AttemptState::Valid);
    let revised = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(revised.len(), 2);
    for (before, after) in initial.iter().zip(&revised) {
        assert_eq!(before.repository_id, after.repository_id);
        assert_eq!(before.number, after.number);
        assert_ne!(before.head_commit, after.head_commit);
        let repo =
            Repository::open_bare(w.forge.repo_path(&format!("org/{}", after.repository_id)))
                .unwrap();
        let commit = repo
            .find_commit(Oid::from_str(&after.head_commit).unwrap())
            .unwrap();
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.author().name().unwrap(), "ArtiDezi");
        let tree = commit.tree().unwrap();
        let entry = tree.get_name("API_VERSION").unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), b"3\n");
    }
    for (index, mr) in revised.iter().enumerate() {
        w.forge
            .merge(
                &format!("org/{}", mr.repository_id),
                mr.number,
                &mr.head_commit.as_str().into(),
                artidezi_forge::MergeMethod::Merge,
            )
            .unwrap();
        assert_eq!(
            w.orchestrator.reconcile_task(&task.id).unwrap(),
            if index == 0 {
                TaskState::AwaitingReview
            } else {
                TaskState::Done
            }
        );
    }
}

#[test]
fn a_failed_cross_repository_check_blocks_every_proposal() {
    let mut w = two_repository_world();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Upgrade both repositories.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("incompatible.sh", &two_repository_script("2", "1"));
    assert_eq!(w.work(&script), AttemptState::Invalid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    assert!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .is_empty()
    );
    for remote in ["org/role", "org/playbook"] {
        assert!(
            w.forge
                .list_open_merge_requests(remote, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            w.forge
                .branch_head(remote, &format!("artidezi/{}", task.id))
                .is_err()
        );
    }
    w.orchestrator
        .request_revision(&task.id, "Adapt the playbook to API 2.", "slavi")
        .unwrap();
    let repair = w.script("repair.sh", &two_repository_script("2", "2"));
    assert_eq!(w.work(&repair), AttemptState::Valid);
    assert_eq!(
        w.orchestrator
            .db
            .list_merge_requests(&task.id)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_foreign_change_in_one_proposal_prevents_rewriting_either_repository() {
    let mut w = two_repository_world();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Upgrade both repositories.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let first = w.script("first.sh", &two_repository_script("2", "2"));
    assert_eq!(w.work(&first), AttemptState::Valid);
    let initial = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    let branch = format!("artidezi/{}", task.id);
    // The role is published second (repository IDs sort playbook, role). Simulate a
    // human changing its branch; the first repository must not get a partial revision.
    let repo = Repository::open_bare(w.forge.repo_path("org/role")).unwrap();
    let main = repo.refname_to_id("refs/heads/main").unwrap();
    repo.reference(&format!("refs/heads/{branch}"), main, true, "human edit")
        .unwrap();
    w.orchestrator
        .request_revision(&task.id, "Use API version 3.", "slavi")
        .unwrap();
    let second = w.script("second.sh", &two_repository_script("3", "3"));
    assert_eq!(w.work(&second), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    assert_eq!(
        w.forge
            .branch_head("org/playbook", &branch)
            .unwrap()
            .as_str(),
        initial
            .iter()
            .find(|m| m.repository_id == "playbook")
            .unwrap()
            .head_commit
    );
    assert_eq!(
        w.forge.branch_head("org/role", &branch).unwrap().as_str(),
        main.to_string()
    );
}

#[test]
fn a_merged_proposal_prevents_rewriting_either_repository() {
    let mut w = two_repository_world();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Upgrade both repositories.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    let first = w.script("first.sh", &two_repository_script("2", "2"));
    assert_eq!(w.work(&first), AttemptState::Valid);
    let initial = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    let merged = initial.iter().find(|m| m.repository_id == "role").unwrap();
    w.forge
        .merge(
            "org/role",
            merged.number,
            &merged.head_commit.as_str().into(),
            artidezi_forge::MergeMethod::Merge,
        )
        .unwrap();
    w.orchestrator
        .request_revision(&task.id, "Use API version 3.", "slavi")
        .unwrap();
    let second = w.script("second.sh", &two_repository_script("3", "3"));
    assert_eq!(w.work(&second), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    for mr in initial {
        assert_eq!(
            w.forge
                .branch_head(&format!("org/{}", mr.repository_id), &mr.source_branch)
                .unwrap()
                .as_str(),
            mr.head_commit
        );
    }
}

#[test]
fn a_partial_publication_failure_preserves_progress_and_routes_to_a_human() {
    let mut w = two_repository_world();
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Upgrade both repositories.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    w.forge.fail_next_create("org/role");
    let script = w.script("first.sh", &two_repository_script("2", "2"));
    // The worker's upload remains accepted; the publication failure belongs to the task.
    assert_eq!(w.work(&script), AttemptState::Valid);
    assert_eq!(
        w.orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::NeedsHuman
    );
    let published = w.orchestrator.db.list_merge_requests(&task.id).unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].repository_id, "playbook");
    assert_eq!(
        w.forge
            .list_open_merge_requests("org/playbook", None)
            .unwrap()
            .len(),
        1
    );
    assert!(
        w.forge
            .list_open_merge_requests("org/role", None)
            .unwrap()
            .is_empty()
    );
    // Do not roll back a successful push or delete evidence of a partially completed operation.
    assert!(
        w.forge
            .branch_head("org/role", &format!("artidezi/{}", task.id))
            .is_ok()
    );
    let events = w.orchestrator.db.list_events(&task.id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "task.needs_human"
            && event
                .payload
                .to_string()
                .contains("simulated merge request creation failure")
    }));
}

#[test]
fn retention_collects_pr_inputs_protects_audit_references_and_runs_on_schedule() {
    let mut w = World::new();
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Retention input fixture",
            "test",
            &TaskSelection::default(),
        )
        .unwrap();
    let mut attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let mut job: artidezi_contract::Job = w.orchestrator.artifacts.get_json(&attempt.job).unwrap();
    let input = w
        .orchestrator
        .artifacts
        .put(b"frozen PR input bundle")
        .unwrap();
    let audited = job.repositories[0].snapshot_bundle.clone();
    w.orchestrator
        .db
        .record_event(
            Some(&task.id),
            None,
            "test",
            "extra.evidence",
            serde_json::json!({"artifact": audited}),
        )
        .unwrap();
    let base_commit = job.repositories[0].base_commit.clone();
    job.repositories[0]
        .input_refs
        .push(artidezi_contract::job::InputRef {
            name: "source".into(),
            commit: base_commit,
            bundle: input.clone(),
        });
    w.orchestrator
        .db
        .cancel_attempt(&attempt.id, "test")
        .unwrap();
    attempt.id = artidezi_store::new_id("att");
    attempt.number = 2;
    job.attempt_id = attempt.id.as_str().into();
    job.attempt_number = 2;
    attempt.job = w.orchestrator.artifacts.put_json(&job).unwrap();
    w.orchestrator.db.insert_attempt(&attempt).unwrap();
    w.orchestrator.close_task(&task.id, "test").unwrap();
    assert!(
        w.orchestrator
            .tick_at(chrono::Utc::now())
            .unwrap()
            .artifact_cleanup
            .is_none()
    );
    let report = w
        .orchestrator
        .tick_at(chrono::Utc::now() + Duration::days(2))
        .unwrap();
    assert!(report.artifact_cleanup.unwrap().removed_bytes > 0);
    assert!(!w.orchestrator.artifacts.contains(&input));
    assert!(w.orchestrator.artifacts.contains(&audited));
}

#[test]
fn retention_preserves_malformed_model_results_without_blocking_cleanup() {
    let mut w = World::new();
    w.orchestrator.config.instance.artifact_retention = Some(retention_policy());
    let task = w
        .orchestrator
        .create_manual_task(
            "roles",
            "Failure evidence",
            "test",
            &TaskSelection::default(),
        )
        .unwrap();
    let script = w.script("invalid-result.sh", &result_writer("{invalid model JSON"));
    assert_eq!(w.work(&script), AttemptState::Invalid);
    let attempt = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
    let result = attempt.result.unwrap();
    let original = w.orchestrator.artifacts.get(&result).unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&original).is_err());
    w.orchestrator.close_task(&task.id, "test").unwrap();
    let report = w
        .orchestrator
        .cleanup_artifacts_at(chrono::Utc::now() + Duration::days(2), true)
        .unwrap();
    assert!(report.removed_bytes > 0);
    assert_eq!(w.orchestrator.artifacts.get(&result).unwrap(), original);
    assert!(
        w.orchestrator
            .artifacts
            .contains(&attempt.attempt_record.unwrap())
    );
}

#[test]
fn model_capacity_failure_schedules_a_retry_and_adopts_old_holds() {
    for enabled_initially in [true, false] {
        let mut w = World::new();
        // Drive a synthetic Codex transcript with the disposable scripted harness.
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
        w.orchestrator
            .config
            .worker_pools
            .get_mut("personal")
            .unwrap()
            .transient_failure_retries = if enabled_initially { 2 } else { 0 };
        let task = w
            .orchestrator
            .create_manual_task(
                "roles",
                "Do the thing.",
                "operator",
                &TaskSelection::default(),
            )
            .unwrap();
        let script = w.script("capacity.sh", r#"printf '%s\n' '{"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}'
exit 1"#);
        assert_eq!(
            w.work_with_record(&script, |record| record.harness.name = "codex".into()),
            AttemptState::Invalid
        );
        if !enabled_initially {
            assert_eq!(
                w.orchestrator.db.get_task(&task.id).unwrap().state,
                TaskState::NeedsHuman
            );
            w.orchestrator
                .config
                .worker_pools
                .get_mut("personal")
                .unwrap()
                .transient_failure_retries = 2;
            w.orchestrator.tick().unwrap();
        }
        let latest = w.orchestrator.db.latest_attempt(&task.id).unwrap().unwrap();
        assert_eq!(latest.number, 2);
        assert_eq!(latest.state, AttemptState::Queued);
        assert!(latest.not_before.unwrap() > chrono::Utc::now());
        assert_eq!(
            w.orchestrator.db.get_task(&task.id).unwrap().state,
            TaskState::Working
        );
        assert_eq!(w.orchestrator.db.list_attempts(&task.id).unwrap().len(), 2);
        w.orchestrator.tick().unwrap();
        assert_eq!(
            w.orchestrator.db.list_attempts(&task.id).unwrap().len(),
            2,
            "recovery must be idempotent"
        );
    }
}
