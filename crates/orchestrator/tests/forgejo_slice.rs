// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The vertical slice against a real Forgejo instead of the fake forge. Runs only when the
//! `ARTIDEZI_FORGEJO_*` variables are set (see `scripts/forgejo-test.sh`).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::TaskSelection;
use artidezi_forge::forgejo::ForgejoForge;
use artidezi_forge::{ForgeAdapter, ForgeError, MergeRequestState as ForgeMrState};
use artidezi_harness::fake::FakeHarness;
use artidezi_orchestrator::Orchestrator;
use artidezi_store::model::*;
use artidezi_worker::{AttemptInputs, WorkerConfig, run_attempt};
use chrono::Duration;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config(url: &str, user: &str, remote: &str) -> String {
    format!(
        r#"
schema_version: 1
instance:
  name: forgejo-test
  bot_identity: {{name: ArtiDezi, email: bot@artidezi.invalid}}
  data_dir: /overridden
  admin_token_ref: admin-token
model_endpoints:
  fake: {{provider: none, auth_mode: none, credential_ref: none}}
worker_pools:
  personal: {{harness_sandbox: disabled, harnesses: [fake], workers: {{w1: {{token_ref: worker-token}}}}}}
agent_profiles:
  fake: {{harness: fake, harness_version_constraint: "*", model: none, model_endpoint: fake, worker_pool: personal}}
forges:
  forgejo: {{provider: forgejo, api_base_url: {url}, credential_ref: forgejo-token, bot_login: {user}}}
projects:
  roles:
    description: One role on a real Forgejo.
    default_agent_profile: fake
    allowed_agent_profiles: [fake]
    default_delivery_mode: merge_request
    allowed_delivery_modes: [merge_request, merge_request_with_automerge, direct_push_fast_forward]
    draft_merge_requests: false
    network:
      command_egress: {{mode: blocked, denied_domains: [], private_network: blocked, allowed_private_endpoints: []}}
      web_search: {{mode: disabled, domain_scope: public_internet}}
    limits: {{wall_clock_seconds: 60, result_json_max_bytes: 65536, max_commits_per_repository: 5, max_diff_bytes_per_repository: 65536, max_commit_message_bytes: 1024}}
    effects: {{merge_renovate_merge_request_on_safe_verdict: false, publish_adaptation_merge_requests: true}}
    repositories:
      role:
        forge: forgejo
        remote: {remote}
        branch: main
        description: The role.
        agent_access: read_write
        verification_recipe:
          - {{name: changes-exist, command: [test, -f, CHANGES.md], timeout_seconds: 10}}
        tagging: {{agent_may_propose: true, allowed_pattern: 'v[0-9]+\.[0-9]+\.[0-9]+-[0-9]+'}}
"#
    )
}

fn result_writer(json: &str) -> String {
    format!(
        "printf '%s' '{}' > \"$ARTIDEZI_RESULT_PATH\"",
        json.replace('\'', "'\\''")
    )
}

fn proposal_script() -> String {
    let json = r#"{"schema_version": 1, "outcome": "proposal", "summary": "Recorded a change on Forgejo.",
 "repositories": [{"id": "role", "changed": true, "head_commit": "HEADSHA", "tag_proposal": "v1.0.0-TAGN"}]}"#;
    format!(
        "echo \"change by $ARTIDEZI_ATTEMPT_ID\" >> CHANGES.md\n\
         git add CHANGES.md\n\
         git commit -q -m \"Record change for $ARTIDEZI_ATTEMPT_ID\"\n\
         HEAD=$(git rev-parse HEAD)\n\
         {}\n\
         sed -i \"s/HEADSHA/$HEAD/; s/TAGN/$$/\" \"$ARTIDEZI_RESULT_PATH\"\n",
        result_writer(json)
    )
}

fn work(
    orchestrator: &mut Orchestrator,
    root: &std::path::Path,
    script: &std::path::Path,
) -> AttemptState {
    let leased = orchestrator
        .lease("personal", "w1", Duration::minutes(5))
        .unwrap()
        .expect("queued attempt");
    let inputs = AttemptInputs {
        job: &leased.job,
        job_bytes: &leased.job_bytes,
        task_document: &leased.task_document,
        snapshot_bundles: leased.snapshot_bundles.clone(),
        input_ref_bundles: leased.input_ref_bundles.clone(),
        previous: leased
            .previous
            .as_ref()
            .map(|p| artidezi_worker::PreviousInputs {
                attempt_document: p.attempt_document.clone(),
                result_document: p.result_document.clone(),
                commit_bundles: p.commit_bundles.clone(),
            }),
    };
    let config = WorkerConfig {
        worker_pool: "personal".into(),
        worker_id: "w1".into(),
        workspaces_root: root.join("workspaces"),
        credentials_root: None,
        max_log_bytes: 65536,
    };
    let harness = FakeHarness {
        script: script.to_path_buf(),
    };
    let output = run_attempt(&config, &harness, &inputs).unwrap();
    let record_bytes = fs::read(output.workspace.join("out/attempt.json")).unwrap();
    let files: Vec<(String, PathBuf)> = output
        .artifacts
        .iter()
        .map(|a| (a.name.clone(), a.path.clone()))
        .collect();
    let state = orchestrator
        .ingest(&leased.attempt.id, "w1", &record_bytes, &files)
        .unwrap();
    output.cleanup().unwrap();
    state
}

#[test]
fn forgejo_vertical_slice() {
    let (Some(url), Some(token), Some(user)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let forge = ForgejoForge::new(&url, &token, &user);
    let remote = forge
        .create_repository(&format!("artidezi-slice-{}", std::process::id()), "main")
        .unwrap();
    let base = forge.branch_head(&remote, "main").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let config = artidezi_config::parse(&config(&url, &user, &remote)).unwrap();
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert(
        "forgejo".into(),
        Box::new(ForgejoForge::new(&url, &token, &user)),
    );
    let mut orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();

    let script = root.join("agent.sh");
    fs::write(&script, format!("#!/bin/sh\nset -e\n{}", proposal_script())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    // Merge request delivery, then a human merges on the forge.
    let task = orchestrator
        .create_manual_task(
            "roles",
            "Record a change.",
            "slavi",
            &TaskSelection::default(),
        )
        .unwrap();
    assert_eq!(task.repositories[0].base_commit, base.as_str());
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task.id).unwrap().state,
        TaskState::AwaitingReview
    );
    let mr = orchestrator
        .db
        .list_merge_requests(&task.id)
        .unwrap()
        .remove(0);
    let info = forge.get_merge_request(&remote, mr.number).unwrap();
    assert_eq!(info.author_login, user);
    assert_eq!(info.head_commit.as_str(), mr.head_commit);
    assert_eq!(info.state, ForgeMrState::Open);
    assert!(
        info.body.contains("Local verification:** passed"),
        "{}",
        info.body
    );
    assert_eq!(
        orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::AwaitingReview
    );
    // Exercise the negotiated force-push lease against real receive-pack, preserving the MR.
    orchestrator
        .request_revision(&task.id, "Revise the recorded change.", "slavi")
        .unwrap();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    let revised = orchestrator
        .db
        .list_merge_requests(&task.id)
        .unwrap()
        .remove(0);
    assert_eq!(revised.number, mr.number);
    assert_ne!(revised.head_commit, mr.head_commit);
    let mr = revised;
    let info = forge.get_merge_request(&remote, mr.number).unwrap();
    assert_eq!(info.head_commit.as_str(), mr.head_commit);
    // Forgejo computes mergeability asynchronously after the revised PR head is pushed.
    let mut merged = false;
    for _ in 0..30 {
        match forge.merge(
            &remote,
            mr.number,
            &info.head_commit,
            artidezi_forge::MergeMethod::Rebase,
        ) {
            Ok(_) => {
                merged = true;
                break;
            }
            Err(ForgeError::NotMergeable(_, reason))
                if reason.contains("Please try again later") =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => panic!("unexpected fixture merge result: {other:?}"),
        }
    }
    assert!(
        merged,
        "Forgejo mergeability should settle within three seconds"
    );
    assert_eq!(
        orchestrator.reconcile_task(&task.id).unwrap(),
        TaskState::Done
    );
    assert_eq!(
        forge.branch_head(&remote, "main").unwrap().as_str(),
        mr.head_commit
    );

    // Automerge: waits for a green status, then merges and tags.
    let selection = TaskSelection {
        agent_profile: None,
        delivery_mode: Some(DeliveryMode::MergeRequestWithAutomerge),
    };
    let task2 = orchestrator
        .create_manual_task("roles", "Record another change.", "slavi", &selection)
        .unwrap();
    assert_eq!(
        task2.repositories[0].base_commit, mr.head_commit,
        "base is re-resolved after the merge"
    );
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    let mr2 = orchestrator
        .db
        .list_merge_requests(&task2.id)
        .unwrap()
        .remove(0);
    assert_eq!(
        orchestrator.reconcile_task(&task2.id).unwrap(),
        TaskState::AwaitingReview
    );
    forge
        .set_commit_status(&remote, &mr2.head_commit, "success", "ci/test")
        .unwrap();
    assert_eq!(
        orchestrator.reconcile_task(&task2.id).unwrap(),
        TaskState::Done
    );
    assert_eq!(
        forge.branch_head(&remote, "main").unwrap().as_str(),
        mr2.head_commit
    );
    let tagged = orchestrator
        .db
        .list_events(&task2.id)
        .unwrap()
        .into_iter()
        .any(|e| e.kind == "tag.created");
    assert!(tagged);

    // Direct push.
    let selection = TaskSelection {
        agent_profile: None,
        delivery_mode: Some(DeliveryMode::DirectPushFastForward),
    };
    let task3 = orchestrator
        .create_manual_task("roles", "Record a third change.", "slavi", &selection)
        .unwrap();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task3.id).unwrap().state,
        TaskState::Done
    );
    assert_ne!(
        forge.branch_head(&remote, "main").unwrap().as_str(),
        mr2.head_commit
    );
}
