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
use artidezi_forge::{
    ChecksState, ForgeAdapter, ForgeError, MergeRequestState as ForgeMrState, NewMergeRequest,
    ReviewState,
};
use artidezi_git::cache::BareCache;
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
    effects: {{merge_source_merge_request_on_safe_verdict: false, publish_adaptation_merge_requests: true}}
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
        system_tmp: None,
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
    assert!(matches!(
        forge.branch_head(&remote, &mr.source_branch),
        Err(ForgeError::NotFound { .. })
    ));
    assert!(
        orchestrator
            .db
            .list_events(&task.id)
            .unwrap()
            .iter()
            .any(|event| event.kind == "merge_request.branch_deleted")
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
    assert!(matches!(
        forge.branch_head(&remote, &mr2.source_branch),
        Err(ForgeError::NotFound { .. })
    ));
    assert!(
        orchestrator
            .db
            .list_events(&task2.id)
            .unwrap()
            .iter()
            .any(|event| event.kind == "merge_request.branch_deleted")
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

/// The same instance shape with discovery that may merge `renovate-bot` updates.
fn source_config(url: &str, user: &str, renovate: &str, remote: &str) -> String {
    let base = config(url, user, remote)
        .replace(
            "merge_source_merge_request_on_safe_verdict: false",
            "merge_source_merge_request_on_safe_verdict: true",
        )
        .replace("        tagging: {agent_may_propose: true, allowed_pattern: 'v[0-9]+\\.[0-9]+\\.[0-9]+-[0-9]+'}\n", "");
    format!(
        r#"{base}    pull_requests:
      poll_interval_seconds: 60
      settle_seconds: 0
      max_runs_per_pull_request: 2
      max_active_tasks: 1
      repositories: [role]
      handlers:
        - id: renovate
          authors: [{renovate}]
          action: assess
          mode: publish
          safe_verdict_action: merge
          require_successful_checks: false
          ci_context: {{results: true}}
          instructions: Assess the update.
"#
    )
}

/// Opens a pull request as the Renovate user, changing one file on a new branch.
fn open_renovate_pull(
    renovate: &ForgejoForge,
    remote: &str,
    cache: &BareCache,
    branch: &str,
    title: &str,
) -> artidezi_forge::MergeRequestInfo {
    let creds = renovate.git_credentials(remote).unwrap();
    cache
        .fetch(
            &renovate.clone_url(remote),
            &["+refs/heads/main:refs/heads/main"],
            creds.as_ref(),
        )
        .unwrap();
    let repo = git2::Repository::open_bare(cache.path()).unwrap();
    let parent = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut tree = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    let blob = repo.blob(format!("{branch}\n").as_bytes()).unwrap();
    tree.insert(format!("{}.txt", branch.replace('/', "-")), blob, 0o100644)
        .unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let sig = git2::Signature::now("Renovate", "renovate@artidezi.invalid").unwrap();
    let commit = repo
        .commit(None, &sig, &sig, title, &tree, &[&parent])
        .unwrap();
    cache
        .set_ref(&format!("refs/heads/{branch}"), commit, "")
        .unwrap();
    cache
        .push(
            &renovate.clone_url(remote),
            &format!("refs/heads/{branch}:refs/heads/{branch}"),
            creds.as_ref(),
        )
        .unwrap();
    renovate
        .create_merge_request(
            remote,
            &NewMergeRequest {
                title: title.into(),
                body: "Automated update.".into(),
                source_branch: branch.into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap()
}

#[test]
fn forgejo_source_approval_and_merge() {
    let (Some(url), Some(token), Some(user), Some(renovate_user), Some(renovate_token)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_TOKEN"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let maintainer_user = env("ARTIDEZI_FORGEJO_MAINTAINER_USER").unwrap();
    let maintainer_token = env("ARTIDEZI_FORGEJO_MAINTAINER_TOKEN").unwrap();
    let forge = ForgejoForge::new(&url, &token, &user);
    let remote = forge
        .create_repository(&format!("artidezi-source-{}", std::process::id()), "main")
        .unwrap();
    forge.add_collaborator(&remote, &renovate_user).unwrap();
    forge.add_collaborator(&remote, &maintainer_user).unwrap();
    let renovate = ForgejoForge::new(&url, &renovate_token, &renovate_user);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let cache = BareCache::open_or_init(&root.join("renovate-cache")).unwrap();
    let config =
        artidezi_config::parse(&source_config(&url, &user, &renovate_user, &remote)).unwrap();
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert(
        "forgejo".into(),
        Box::new(
            ForgejoForge::new(&url, &token, &user).with_source_actions(
                [remote.clone()],
                [(remote.clone(), renovate_user.clone())]
                    .into_iter()
                    .collect(),
            ),
        ),
    );
    let mut orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();
    let script = root.join("unchanged.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -e\n{}\n",
            result_writer(
                r#"{"schema_version":1,"outcome":"approve_untouched","summary":"No adaptations necessary.","repositories":[{"id":"role","changed":false}]}"#
            )
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    // Green checks and no objection: ArtiDezi approves, then merges the exact assessed head.
    let pull = open_renovate_pull(&renovate, &remote, &cache, "renovate/lib", "Update library");
    forge
        .set_commit_status(&remote, pull.head_commit.as_str(), "success", "ci/test")
        .unwrap();
    assert_eq!(
        forge
            .get_merge_request(&remote, pull.number)
            .unwrap()
            .checks,
        ChecksState::Success
    );
    let queued = orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(queued.errors.is_empty(), "{:?}", queued.errors);
    let task = queued.queued[0].clone();
    let source =
        artidezi_orchestrator::discovery::source_of(&orchestrator.db.get_task(&task).unwrap())
            .unwrap()
            .unwrap();
    let snapshot = source.ci_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.results.len(), 1);
    assert_eq!(snapshot.results[0].name, "ci/test");
    assert_eq!(snapshot.results[0].status, "success");
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task).unwrap().state,
        TaskState::Done
    );
    let merged = forge.get_merge_request(&remote, pull.number).unwrap();
    assert_eq!(merged.state, ForgeMrState::Merged);
    assert_eq!(merged.head_commit, pull.head_commit);
    assert_eq!(
        Some(forge.branch_head(&remote, "main").unwrap()),
        merged.merge_commit
    );
    let reviews = forge.list_reviews(&remote, pull.number).unwrap();
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0].author_login, user);
    assert_eq!(reviews[0].state, ReviewState::Approved);
    assert_eq!(reviews[0].head_commit, pull.head_commit);
    let events: Vec<String> = orchestrator
        .db
        .list_events(&task)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for kind in [
        "assessment.approval_planned",
        "source.merge_planned",
        "source.merge_accepted",
        "source.merge_observed",
    ] {
        assert!(events.iter().any(|e| e == kind), "{kind}: {events:?}");
    }

    // A maintainer's change request holds an otherwise eligible update without approving it.
    let objected = open_renovate_pull(&renovate, &remote, &cache, "renovate/held", "Update held");
    forge
        .set_commit_status(&remote, objected.head_commit.as_str(), "success", "ci/test")
        .unwrap();
    let maintainer = ForgejoForge::new(&url, &maintainer_token, &maintainer_user);
    maintainer
        .submit_review_for_test(
            &remote,
            objected.number,
            &objected.head_commit,
            "REQUEST_CHANGES",
            "Hold this.",
        )
        .unwrap();
    let queued = orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(queued.errors.is_empty(), "{:?}", queued.errors);
    let held = queued.queued[0].clone();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&held).unwrap().state,
        TaskState::NeedsHuman
    );
    let hold = orchestrator
        .db
        .list_events(&held)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "task.needs_human")
        .unwrap();
    assert!(
        hold.payload["reason"]
            .as_str()
            .unwrap()
            .contains("requested changes"),
        "{}",
        hold.payload
    );
    assert_eq!(
        forge
            .get_merge_request(&remote, objected.number)
            .unwrap()
            .state,
        ForgeMrState::Open
    );
    assert!(
        forge
            .list_reviews(&remote, objected.number)
            .unwrap()
            .iter()
            .all(|r| r.state != ReviewState::Approved)
    );
}

/// A repository without CI: Forgejo reports no checks at all for the Renovate head.
#[test]
fn forgejo_checkless_source_merges_only_with_the_opt_in() {
    let (Some(url), Some(token), Some(user), Some(renovate_user), Some(renovate_token)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_TOKEN"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let forge = ForgejoForge::new(&url, &token, &user);
    let remote = forge
        .create_repository(
            &format!("artidezi-checkless-{}", std::process::id()),
            "main",
        )
        .unwrap();
    forge.add_collaborator(&remote, &renovate_user).unwrap();
    let renovate = ForgejoForge::new(&url, &renovate_token, &renovate_user);
    let dir = tempfile::tempdir().unwrap();
    let cache = BareCache::open_or_init(&dir.path().join("renovate-cache")).unwrap();
    let pull = open_renovate_pull(&renovate, &remote, &cache, "renovate/no-ci", "Update lib");
    assert_eq!(
        forge
            .get_merge_request(&remote, pull.number)
            .unwrap()
            .checks,
        ChecksState::Absent
    );

    // Each phase is a separate controller with its own database, built from configuration the
    // way the server builds it, so the adapter's own gate follows the handler too.
    let controller = |phase: &str, opted_in: bool| {
        let root = dir.path().join(phase);
        fs::create_dir_all(&root).unwrap();
        let mut yaml = source_config(&url, &user, &renovate_user, &remote);
        if opted_in {
            yaml = yaml.replace(
                "          safe_verdict_action: merge\n",
                "          safe_verdict_action: merge\n          safe_verdict_checks: passing_or_none\n",
            );
        }
        let config = artidezi_config::parse(&yaml).unwrap();
        assert!(
            artidezi_config::validate(&config).is_empty(),
            "{:?}",
            artidezi_config::validate(&config)
        );
        let forges = artidezi_orchestrator::forges::build_forges(
            &config,
            &forgejo_secrets(&root, &token),
            &root.join("fake"),
        )
        .unwrap();
        let orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();
        let script = root.join("unchanged.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -e\n{}\n",
                result_writer(
                    r#"{"schema_version":1,"outcome":"approve_untouched","summary":"No adaptations necessary.","repositories":[{"id":"role","changed":false}]}"#
                )
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (orchestrator, root, script)
    };

    // Without the opt-in, an unchanged verdict on a check-less head waits for a person.
    let (mut orchestrator, root, script) = controller("plain", false);
    let queued = orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(queued.errors.is_empty(), "{:?}", queued.errors);
    let task = queued.queued[0].clone();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task).unwrap().state,
        TaskState::NeedsHuman
    );
    let hold = orchestrator
        .db
        .list_events(&task)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "task.needs_human")
        .unwrap();
    assert!(
        hold.payload["reason"]
            .as_str()
            .unwrap()
            .contains("No forge checks were reported"),
        "{}",
        hold.payload
    );
    assert_eq!(
        forge.get_merge_request(&remote, pull.number).unwrap().state,
        ForgeMrState::Open
    );
    assert!(forge.list_reviews(&remote, pull.number).unwrap().is_empty());

    // With it, the same head is approved and merged, and the receipt says checks were absent.
    let (mut orchestrator, root, script) = controller("opted-in", true);
    let queued = orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(queued.errors.is_empty(), "{:?}", queued.errors);
    let task = queued.queued[0].clone();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task).unwrap().state,
        TaskState::Done
    );
    let merged = forge.get_merge_request(&remote, pull.number).unwrap();
    assert_eq!(merged.state, ForgeMrState::Merged);
    assert_eq!(merged.head_commit, pull.head_commit);
    let reviews = forge.list_reviews(&remote, pull.number).unwrap();
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0].state, ReviewState::Approved);
    assert!(
        reviews[0]
            .body
            .contains("No forge checks were reported for this commit"),
        "{}",
        reviews[0].body
    );
    let events = orchestrator.db.list_events(&task).unwrap();
    for kind in ["assessment.approval_planned", "source.merge_planned"] {
        let event = events.iter().find(|e| e.kind == kind).unwrap();
        assert_eq!(event.payload["forge_checks"], "none", "{kind}");
    }
    assert!(events.iter().any(|e| e.kind == "source.merge_observed"));
}

#[test]
fn forgejo_adaptation_merges_once_its_checks_pass() {
    let (Some(url), Some(token), Some(user), Some(renovate_user), Some(renovate_token)) = (
        env("ARTIDEZI_FORGEJO_URL"),
        env("ARTIDEZI_FORGEJO_TOKEN"),
        env("ARTIDEZI_FORGEJO_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_USER"),
        env("ARTIDEZI_FORGEJO_RENOVATE_TOKEN"),
    ) else {
        eprintln!("skipping: ARTIDEZI_FORGEJO_* not set");
        return;
    };
    let maintainer_user = env("ARTIDEZI_FORGEJO_MAINTAINER_USER").unwrap();
    let maintainer_token = env("ARTIDEZI_FORGEJO_MAINTAINER_TOKEN").unwrap();
    let forge = ForgejoForge::new(&url, &token, &user);
    let remote = forge
        .create_repository(&format!("artidezi-adapt-{}", std::process::id()), "main")
        .unwrap();
    forge.add_collaborator(&remote, &renovate_user).unwrap();
    forge.add_collaborator(&remote, &maintainer_user).unwrap();
    let renovate = ForgejoForge::new(&url, &renovate_token, &renovate_user);
    let maintainer = ForgejoForge::new(&url, &maintainer_token, &maintainer_user);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let cache = BareCache::open_or_init(&root.join("renovate-cache")).unwrap();
    let yaml = source_config(&url, &user, &renovate_user, &remote)
        .replace(
            "publish_adaptation_merge_requests: true}",
            "publish_adaptation_merge_requests: true, merge_adaptation_merge_requests: true}",
        )
        .replace(
            "          safe_verdict_action: merge\n",
            "          safe_verdict_action: merge\n          adaptation_action: merge\n",
        );
    let config = artidezi_config::parse(&yaml).unwrap();
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
    let forges: BTreeMap<String, Box<dyn ForgeAdapter>> =
        artidezi_orchestrator::forges::build_forges(
            &config,
            &forgejo_secrets(&root, &token),
            &root.join("fake"),
        )
        .unwrap();
    let mut orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();
    let script = root.join("adapt.sh");
    let json = r#"{"schema_version": 1, "outcome": "proposal", "summary": "Adapted the role.",
 "repositories": [{"id": "role", "changed": true, "head_commit": "HEADSHA"}]}"#;
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -e\necho adapted >> CHANGES.md\ngit add CHANGES.md\n\
             git commit -q -m 'Adapt the role'\nHEAD=$(git rev-parse HEAD)\n{}\n\
             sed -i \"s/HEADSHA/$HEAD/\" \"$ARTIDEZI_RESULT_PATH\"\n",
            result_writer(json)
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let pull = open_renovate_pull(&renovate, &remote, &cache, "renovate/adapt", "Update lib");
    forge
        .set_commit_status(&remote, pull.head_commit.as_str(), "success", "ci/test")
        .unwrap();
    let queued = orchestrator
        .scan_pull_requests(Some("roles"), true)
        .unwrap();
    assert!(queued.errors.is_empty(), "{:?}", queued.errors);
    let task = queued.queued[0].clone();
    assert_eq!(work(&mut orchestrator, &root, &script), AttemptState::Valid);
    assert_eq!(
        orchestrator.db.get_task(&task).unwrap().state,
        TaskState::AwaitingReview
    );
    let [adaptation] = orchestrator
        .db
        .list_merge_requests(&task)
        .unwrap()
        .try_into()
        .unwrap();
    let info = forge.get_merge_request(&remote, adaptation.number).unwrap();
    assert!(
        info.body
            .contains("ArtiDezi merges this once its checks pass")
    );
    let head = info.head_commit.clone();

    // No checks yet, then a maintainer's objection: the adaptation waits for a person.
    assert_eq!(
        orchestrator.reconcile_task(&task).unwrap(),
        TaskState::AwaitingReview
    );
    forge
        .set_commit_status(&remote, head.as_str(), "success", "ci/test")
        .unwrap();
    maintainer
        .submit_review_for_test(
            &remote,
            adaptation.number,
            &head,
            "REQUEST_CHANGES",
            "Not yet.",
        )
        .unwrap();
    assert_eq!(
        orchestrator.reconcile_task(&task).unwrap(),
        TaskState::AwaitingReview
    );
    assert_eq!(
        forge
            .get_merge_request(&remote, adaptation.number)
            .unwrap()
            .state,
        ForgeMrState::Open
    );

    // Once the maintainer's latest decision approves and checks pass, ArtiDezi merges it.
    maintainer
        .submit_review_for_test(&remote, adaptation.number, &head, "APPROVED", "Fine now.")
        .unwrap();
    assert_eq!(orchestrator.reconcile_task(&task).unwrap(), TaskState::Done);
    let merged = forge.get_merge_request(&remote, adaptation.number).unwrap();
    assert_eq!(merged.state, ForgeMrState::Merged);
    assert_eq!(merged.head_commit, head);
    assert_eq!(
        Some(forge.branch_head(&remote, "main").unwrap()),
        merged.merge_commit
    );
    let events: Vec<String> = orchestrator
        .db
        .list_events(&task)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for kind in ["adaptation.merge_planned", "merge_request.automerged"] {
        assert!(events.iter().any(|e| e == kind), "{kind}: {events:?}");
    }
}

fn forgejo_secrets(
    root: &std::path::Path,
    token: &str,
) -> artidezi_orchestrator::secrets::SecretsDir {
    let dir = root.join("secrets");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("forgejo-token"), token).unwrap();
    artidezi_orchestrator::secrets::SecretsDir::new(&dir)
}
