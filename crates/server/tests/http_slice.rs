// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The vertical slice over HTTP: an orchestrator served by axum on a random port, a task created
//! through the admin API, and the standalone worker daemon leasing, running (fake harness),
//! uploading and reporting through the worker API. Also the token boundaries.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use artidezi_forge::ForgeAdapter;
use artidezi_forge::fake::FakeForge;
use artidezi_orchestrator::Orchestrator;
use artidezi_server::{AppState, SharedState, Tokens, router_with_ui, ui::UiConfig};
use artidezi_store::db::{UiGrant, UiUser};
use artidezi_worker::{Daemon, DaemonConfig, WorkerConfig};
use git2::{Oid, Repository};

#[path = "support/responsiveness.rs"]
mod responsiveness;

const CONFIG: &str = r#"
schema_version: 1
instance:
  name: http-test
  bot_identity: {name: ArtiDezi, email: bot@artidezi.invalid}
  data_dir: /overridden
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
    description: A role.
    default_agent_profile: fake
    allowed_agent_profiles: [fake]
    default_delivery_mode: merge_request
    allowed_delivery_modes: [merge_request]
    draft_merge_requests: false
    network:
      command_egress: {mode: blocked, denied_domains: [], private_network: blocked, allowed_private_endpoints: []}
      web_search: {mode: disabled, domain_scope: public_internet}
    limits: {wall_clock_seconds: 60, result_json_max_bytes: 65536, max_commits_per_repository: 5, max_diff_bytes_per_repository: 65536, max_commit_message_bytes: 1024}
    effects: {merge_source_merge_request_on_safe_verdict: false, publish_adaptation_merge_requests: true}
    repositories:
      role:
        forge: fake
        remote: org/role
        branch: main
        description: The role.
        agent_access: read_write
        verification_recipe:
          - {name: changes-exist, command: [test, -f, CHANGES.md], timeout_seconds: 10}
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

fn proposal_script() -> String {
    let json = r#"{"schema_version": 1, "outcome": "proposal", "summary": "Recorded a change over HTTP.",
 "repositories": [{"id": "role", "changed": true, "head_commit": "HEADSHA"}]}"#;
    format!(
        "#!/bin/sh\nset -e\n\
         echo \"change by $ARTIDEZI_ATTEMPT_ID\" >> CHANGES.md\n\
         git add CHANGES.md\n\
         git commit -q -m \"Record change for $ARTIDEZI_ATTEMPT_ID\"\n\
         HEAD=$(git rev-parse HEAD)\n\
         printf '%s' '{}' > \"$ARTIDEZI_RESULT_PATH\"\n\
         sed -i \"s/HEADSHA/$HEAD/\" \"$ARTIDEZI_RESULT_PATH\"\n",
        json.replace('\'', "'\\''")
    )
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    base_url: String,
    forge: FakeForge,
    state: SharedState,
    gate: Arc<responsiveness::Gate>,
}

async fn world() -> World {
    world_with_ui(None).await
}

async fn world_with_ui(ui: Option<UiConfig>) -> World {
    world_with_lease_ttl(ui, Duration::from_secs(60)).await
}

async fn world_with_lease_ttl(ui: Option<UiConfig>, lease_ttl: Duration) -> World {
    world_configured(ui, lease_ttl, |_| {}).await
}

async fn world_with_setup(ui: Option<UiConfig>, setup: impl FnOnce(&mut Orchestrator)) -> World {
    world_configured(ui, Duration::from_secs(60), setup).await
}

async fn world_configured(
    ui: Option<UiConfig>,
    lease_ttl: Duration,
    setup: impl FnOnce(&mut Orchestrator),
) -> World {
    world_configured_with(
        ui,
        lease_ttl,
        artidezi_config::parse(CONFIG).unwrap(),
        setup,
    )
    .await
}

async fn world_configured_with(
    ui: Option<UiConfig>,
    lease_ttl: Duration,
    config: artidezi_config::Config,
    setup: impl FnOnce(&mut Orchestrator),
) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let forge = FakeForge::new(&root.join("forge"));
    let repo = forge.ensure_repository("org/role").unwrap();
    seed(&repo, None, "README.md", "role\n", "Initial");
    let other_repo = forge.ensure_repository("org/other").unwrap();
    seed(&other_repo, None, "README.md", "other\n", "Initial");

    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    let gate = Arc::new(responsiveness::Gate::default());
    forges.insert(
        "fake".into(),
        Box::new(responsiveness::GatedForge {
            inner: forge.clone(),
            gate: gate.clone(),
        }),
    );
    let mut orchestrator = Orchestrator::open(config, &root.join("data"), forges).unwrap();
    setup(&mut orchestrator);
    if ui.is_some() {
        orchestrator
            .db
            .create_ui_user(&test_ui_user(
                "test-admin",
                "Test admin",
                true,
                BTreeMap::new(),
            ))
            .unwrap();
    }
    let state: SharedState = Arc::new(AppState::new(
        orchestrator,
        Tokens {
            workers: vec![
                worker_token("w1", "worker-secret"),
                worker_token("w2", "worker-2-secret"),
            ],
            admin: "admin-secret".into(),
        },
        lease_ttl,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let app = router_with_ui(state.clone(), ui);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    World {
        _dir: dir,
        root,
        base_url,
        forge,
        state,
        gate,
    }
}

fn worker_token(id: &str, token: &str) -> artidezi_server::WorkerToken {
    artidezi_server::WorkerToken {
        worker_pool: "personal".into(),
        worker_id: id.into(),
        token: token.into(),
    }
}

async fn reopen_with_ui(w: &World, config: artidezi_config::Config, ui: UiConfig) -> String {
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert("fake".into(), Box::new(w.forge.clone()));
    let state = Arc::new(AppState::new(
        Orchestrator::open(config, &w.root.join("data"), forges).unwrap(),
        Tokens {
            workers: Vec::new(),
            admin: "admin-secret".into(),
        },
        Duration::from_secs(60),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router_with_ui(state, Some(ui));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn admin(
    base: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    token: &str,
) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{base}{path}");
    let mut response = match (method, body) {
        ("GET", _) => agent
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .unwrap(),
        ("POST", Some(b)) => agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .send_json(b)
            .unwrap(),
        ("POST", None) => agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .send_empty()
            .unwrap(),
        _ => unreachable!(),
    };
    let status = response.status().as_u16();
    let bytes = response.body_mut().read_to_vec().unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

fn daemon(w: &World, token: &str) -> Daemon {
    let script = w.root.join("agent.sh");
    fs::write(&script, proposal_script()).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    Daemon::new(DaemonConfig {
        orchestrator_url: w.base_url.clone(),
        allow_insecure_orchestrator_url: false,
        token: token.into(),
        worker: WorkerConfig {
            worker_pool: "personal".into(),
            worker_id: "w1".into(),
            workspaces_root: w.root.join("workspaces"),
            credentials_root: None,
            max_log_bytes: 65536,
        },
        poll_interval: Duration::from_millis(10),
        tags: vec![],
        codex_binary: None,
        pi_binary: None,
        fake_script: Some(script),
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_daemon_completes_a_task_over_http() {
    let w = world().await;
    let base = w.base_url.clone();

    // Admin creates a task.
    let (status, task) = admin(
        &base,
        "POST",
        "/admin/v1/tasks",
        Some(
            serde_json::json!({"project": "roles", "instructions": "Record a change.", "created_by": "slavi"}),
        ),
        "admin-secret",
    );
    assert_eq!(status, 200, "{task}");
    let task_id = task["id"].as_str().unwrap().to_owned();
    assert_eq!(task["state"], "working");

    // The worker daemon does one iteration against the live server.
    let d = daemon(&w, "worker-secret");
    let had_work = tokio::task::spawn_blocking(move || d.run_once())
        .await
        .unwrap()
        .unwrap();
    assert!(had_work);
    assert!(
        !w.state
            .orchestrator
            .lock()
            .unwrap()
            .db
            .artifact_uploads()
            .unwrap()
            .is_empty()
    );

    let (status, detail) = admin(
        &base,
        "GET",
        &format!("/admin/v1/tasks/{task_id}"),
        None,
        "admin-secret",
    );
    assert_eq!(status, 200);
    assert_eq!(detail["task"]["state"], "awaiting_review", "{detail}");
    assert_eq!(
        detail["attempts"][0]["state"], "valid",
        "{}",
        detail["attempts"][0]
    );
    assert_eq!(detail["merge_requests"].as_array().unwrap().len(), 1);
    let mr_number = detail["merge_requests"][0]["number"].as_u64().unwrap();
    let info = w.forge.get_merge_request("org/role", mr_number).unwrap();
    assert!(
        info.body.contains("Recorded a change over HTTP"),
        "{}",
        info.body
    );
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"attempt.leased"), "{kinds:?}");
    assert!(kinds.contains(&"merge_request.published"), "{kinds:?}");

    // Nothing left to lease; the workspace was destroyed.
    let d = daemon(&w, "worker-secret");
    assert!(
        !tokio::task::spawn_blocking(move || d.run_once())
            .await
            .unwrap()
            .unwrap()
    );
    assert!(
        fs::read_dir(w.root.join("workspaces"))
            .unwrap()
            .flatten()
            .all(|e| e.file_name() == "inputs")
    );

    // Revision through the admin API, processed by another daemon iteration.
    let (status, attempt) = admin(
        &base,
        "POST",
        &format!("/admin/v1/tasks/{task_id}/revisions"),
        Some(serde_json::json!({"feedback": "Once more, please.", "actor": "slavi"})),
        "admin-secret",
    );
    assert_eq!(status, 200, "{attempt}");
    assert_eq!(attempt["number"], 2);
    let d = daemon(&w, "worker-secret");
    assert!(
        tokio::task::spawn_blocking(move || d.run_once())
            .await
            .unwrap()
            .unwrap()
    );
    let (_, detail) = admin(
        &base,
        "GET",
        &format!("/admin/v1/tasks/{task_id}"),
        None,
        "admin-secret",
    );
    assert_eq!(detail["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(detail["attempts"][1]["state"], "valid");
    assert_eq!(detail["task"]["state"], "awaiting_review");

    // A human merges on the forge; the maintenance tick notices.
    let info = w.forge.get_merge_request("org/role", mr_number).unwrap();
    w.forge
        .merge(
            "org/role",
            mr_number,
            &info.head_commit,
            artidezi_forge::MergeMethod::Merge,
        )
        .unwrap();
    let (status, report) = admin(&base, "POST", "/admin/v1/tick", None, "admin-secret");
    assert_eq!(status, 200, "{report}");
    let (_, detail) = admin(
        &base,
        "GET",
        &format!("/admin/v1/tasks/{task_id}"),
        None,
        "admin-secret",
    );
    assert_eq!(detail["task"]["state"], "done");

    // Closing a done task is rejected (terminal state).
    let (status, _) = admin(
        &base,
        "POST",
        &format!("/admin/v1/tasks/{task_id}/close"),
        Some(serde_json::json!({"actor": "slavi"})),
        "admin-secret",
    );
    assert_eq!(status, 409);
    let _ = &w.state;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_task_list_totals_recorded_usage_across_attempts() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let (status, task) = admin(
        &w.base_url,
        "POST",
        "/admin/v1/tasks",
        Some(
            serde_json::json!({"project": "roles", "instructions": "Record usage.", "created_by": "test-admin"}),
        ),
        "admin-secret",
    );
    assert_eq!(status, 200);
    let task_id = task["id"].as_str().unwrap();
    let cookie = login_browser(&w.base_url);
    let (_, _, pending) = browser_request(&w.base_url, "/tasks", Some(&cookie), None, None);
    assert!(!pending.contains("Total input across recorded attempts"));

    for (input, output, expected_input, expected_output) in
        [(1234, 56, "1.2k", "56"), (2000, 200, "3.2k", "256")]
    {
        let worker = daemon(&w, "worker-secret");
        let script = w.root.join("agent.sh");
        let mut source = fs::read_to_string(&script).unwrap();
        source.push_str(&format!(
            "\necho input_tokens={input}\necho output_tokens={output}\n"
        ));
        fs::write(&script, source).unwrap();
        assert!(
            tokio::task::spawn_blocking(move || worker.run_once())
                .await
                .unwrap()
                .unwrap()
        );
        let (_, _, page) = browser_request(&w.base_url, "/tasks", Some(&cookie), None, None);
        assert!(page.contains(&format!("↑ {expected_input} tok.")));
        assert!(page.contains(&format!("↓ {expected_output} tok.")));
        if input == 1234 {
            let (status, _) = admin(
                &w.base_url,
                "POST",
                &format!("/admin/v1/tasks/{task_id}/revisions"),
                Some(serde_json::json!({"feedback": "Another pass.", "actor": "test-admin"})),
                "admin-secret",
            );
            assert_eq!(status, 200);
        }
    }

    let (status, _) = admin(
        &w.base_url,
        "POST",
        "/admin/v1/tasks",
        Some(
            serde_json::json!({"project": "roles", "instructions": "No model tokens.", "created_by": "test-admin"}),
        ),
        "admin-secret",
    );
    assert_eq!(status, 200);
    let worker = daemon(&w, "worker-secret");
    let script = w.root.join("agent.sh");
    let mut source = fs::read_to_string(&script).unwrap();
    source.push_str("\necho input_tokens=0\necho output_tokens=0\n");
    fs::write(&script, source).unwrap();
    assert!(
        tokio::task::spawn_blocking(move || worker.run_once())
            .await
            .unwrap()
            .unwrap()
    );
    let (_, _, page) = browser_request(&w.base_url, "/tasks", Some(&cookie), None, None);
    assert!(page.contains(r#"title="Total input across recorded attempts: 0 tokens">↑ 0 tok."#));
    assert!(page.contains(r#"title="Total output across recorded attempts: 0 tokens">↓ 0 tok."#));
}

#[tokio::test(flavor = "multi_thread")]
async fn execution_deadline_refuses_worker_traffic_before_maintenance_runs() {
    let w = world_with_setup(None, |o| {
        o.create_manual_task(
            "roles",
            "Inspect an application update",
            "test",
            &Default::default(),
        )
        .unwrap();
        // Simulate an old controller that kept extending an unhealthy worker's lease.
        o.db.lease_next_attempt(
            "personal",
            "w1",
            chrono::Duration::hours(3),
            chrono::Utc::now() - chrono::Duration::hours(2),
        )
        .unwrap()
        .unwrap();
    })
    .await;
    let (attempt, job, original_expiry) = {
        let o = w.state.orchestrator.lock().unwrap();
        let task = o.db.list_tasks(None).unwrap().remove(0);
        let attempt = o.db.latest_attempt(&task.id).unwrap().unwrap();
        let job = o.load_job(&attempt).unwrap();
        let expiry = o.db.get_lease(&attempt.id).unwrap().unwrap().expires_at;
        (attempt, job, expiry)
    };
    for (path, body) in [
        (
            format!("/worker/v1/attempts/{}/heartbeat", attempt.id),
            serde_json::json!({"worker_id":"w1"}),
        ),
        (
            format!("/worker/v1/attempts/{}/record", attempt.id),
            serde_json::json!({"worker_id":"w1", "record":{}, "artifacts":{}}),
        ),
    ] {
        let (status, body) = admin(&w.base_url, "POST", &path, Some(body), "worker-secret");
        assert_eq!(status, 409, "{body}");
    }
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let path = format!(
        "{}/worker/v1/attempts/{}/artifacts/{}",
        w.base_url, attempt.id, job.task_document.sha256
    );
    let response = agent
        .get(&path)
        .header("Authorization", "Bearer worker-secret")
        .call()
        .unwrap();
    assert_eq!(response.status(), 409);
    // An exact-hash upload is also rejected; otherwise it could leave stale accepted evidence.
    let response = agent.put(format!("{}/worker/v1/attempts/{}/artifacts/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", w.base_url, attempt.id))
        .header("Authorization", "Bearer worker-secret").send(&[] as &[u8]).unwrap();
    assert_eq!(response.status(), 409);
    let o = w.state.orchestrator.lock().unwrap();
    assert_eq!(
        o.db.get_attempt(&attempt.id).unwrap().state,
        artidezi_store::model::AttemptState::Leased
    );
    assert_eq!(
        o.db.get_lease(&attempt.id).unwrap().unwrap().expires_at,
        original_expiry
    );
    assert!(
        o.db.list_merge_requests(&attempt.task_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_running_task_stops_the_harness_without_uploading_or_publishing() {
    let w = world_with_lease_ttl(None, Duration::from_secs(3)).await;
    let (status, task) = admin(
        &w.base_url,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project":"roles", "instructions":"Wait for cancellation."})),
        "admin-secret",
    );
    assert_eq!(status, 200, "{task}");
    let task_id = task["id"].as_str().unwrap();
    let d = daemon(&w, "worker-secret");
    fs::write(w.root.join("agent.sh"), "#!/bin/sh\necho started > \"$ARTIDEZI_WORKSPACE/out/started\"\necho partial-output\nsleep 60\necho should-not-finish > \"$ARTIDEZI_WORKSPACE/out/finished\"\n").unwrap();
    let worker = tokio::task::spawn_blocking(move || d.run_once());
    let attempt = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .db
        .latest_attempt(task_id)
        .unwrap()
        .unwrap();
    let workspace = w.root.join("workspaces").join(&attempt.id);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !workspace.join("out/started").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (status, _) = admin(
        &w.base_url,
        "POST",
        &format!("/admin/v1/tasks/{task_id}/close"),
        Some(serde_json::json!({"actor":"test-admin"})),
        "admin-secret",
    );
    assert_eq!(status, 200);
    assert!(
        tokio::time::timeout(Duration::from_secs(15), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert!(!workspace.join("out/finished").exists());
    assert!(!workspace.join("out/attempt.json").exists());
    assert!(
        fs::read_to_string(workspace.join("out/harness-stdout.log"))
            .unwrap()
            .contains("partial-output")
    );
    let (_, detail) = admin(
        &w.base_url,
        "GET",
        &format!("/admin/v1/tasks/{task_id}"),
        None,
        "admin-secret",
    );
    assert_eq!(detail["task"]["state"], "closed");
    assert_eq!(detail["attempts"][0]["state"], "cancelled");
    assert!(detail["attempts"][0]["attempt_record"].is_null());
    assert!(detail["merge_requests"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_are_authenticated_and_limited() {
    use sha2::Digest;

    let mut config = artidezi_config::parse(CONFIG).unwrap();
    config.instance.limits.max_upload_bytes = 8;
    config.instance.limits.max_attempt_upload_bytes = 12;
    let w = world_configured_with(None, Duration::from_secs(120), config, |o| {
        o.create_manual_task("roles", "Inspect", "test", &Default::default())
            .unwrap();
        o.db.lease_next_attempt(
            "personal",
            "w1",
            chrono::Duration::hours(1),
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
    })
    .await;
    let attempt = {
        let o = w.state.orchestrator.lock().unwrap();
        let task = o.db.list_tasks(None).unwrap().remove(0);
        o.db.latest_attempt(&task.id).unwrap().unwrap()
    };
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let put = |bytes: &[u8], token: &str| {
        let sha256 = hex::encode(sha2::Sha256::digest(bytes));
        let url = format!(
            "{}/worker/v1/attempts/{}/artifacts/{sha256}",
            w.base_url, attempt.id
        );
        agent
            .put(url)
            .header("Authorization", &format!("Bearer {token}"))
            .send(bytes)
            .unwrap()
            .status()
            .as_u16()
    };

    assert_eq!(put(b"12345678", "wrong"), 403);
    assert_eq!(put(b"123456789", "worker-secret"), 413);
    assert_eq!(put(b"12345678", "worker-secret"), 200);
    assert_eq!(put(b"12345678", "worker-secret"), 200);
    assert_eq!(put(b"abcdefgh", "worker-secret"), 413);

    let temp = w.root.join("data/artifacts/tmp");
    assert_eq!(std::fs::read_dir(temp).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_workers_leave_attempts_queued_for_capable_ones() {
    let w = world_with_setup(None, |o| {
        o.create_manual_task("roles", "Inspect", "test", &Default::default())
            .unwrap();
    })
    .await;
    let lease = |name: &str| {
        serde_json::json!({
            "worker_pool": "personal",
            "worker_id": "w1",
            "supported_job_schema_versions": artidezi_contract::job::SUPPORTED_JOB_SCHEMA_VERSIONS,
            "harnesses": [{"name": name, "version": "1.0.0"}],
        })
    };

    let (status, _) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease("codex")),
        "worker-secret",
    );
    assert_eq!(status, 204);

    let (status, leased) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease("fake")),
        "worker-secret",
    );
    assert_eq!(status, 200, "{leased}");
}

#[tokio::test(flavor = "multi_thread")]
async fn required_tags_route_work_and_explain_unschedulable_tasks() {
    let mut config = artidezi_config::parse(CONFIG).unwrap();
    config
        .projects
        .get_mut("roles")
        .unwrap()
        .required_worker_tags = vec!["docker-support".into()];
    let w = world_configured_with(
        Some(UiConfig::new(false)),
        Duration::from_secs(60),
        config,
        |_| {},
    )
    .await;
    let task = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .create_manual_task("roles", "Inspect", "admin", &Default::default())
        .unwrap();
    let lease = |tags: &[&str]| {
        let body = serde_json::json!({
            "worker_pool": "personal",
            "worker_id": "w1",
            "supported_job_schema_versions": artidezi_contract::job::SUPPORTED_JOB_SCHEMA_VERSIONS,
            "harnesses": [{"name": "fake", "version": "1.0.0"}],
            "tags": tags,
        });
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/lease",
            Some(body),
            "worker-secret",
        )
        .0
    };

    assert_eq!(lease(&["rust-support"]), 204);
    let cookie = login_browser(&w.base_url);
    let (_, _, page) = browser_request(
        &w.base_url,
        &format!("/tasks/{}", task.number),
        Some(&cookie),
        None,
        None,
    );
    assert!(page.contains("No connected worker has the required tags: docker-support."));

    assert_eq!(lease(&["not a tag"]), 400);
    assert_eq!(lease(&["rust-support", "docker-support"]), 200);

    let tag = |page: &str, name: &str| {
        let end = page.find(&format!(r#"">{name}</span>"#)).unwrap();
        page[page[..end].rfind("tag-").unwrap()..end].to_owned()
    };
    let (_, _, workers) = browser_request(&w.base_url, "/workers", Some(&cookie), None, None);
    let (_, _, projects) = browser_request(&w.base_url, "/projects", Some(&cookie), None, None);
    assert_eq!(
        tag(&workers, "docker-support"),
        tag(&projects, "docker-support")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_scoped() {
    let w = world().await;
    let base = w.base_url.clone();

    for (method, path, body) in [
        ("GET", "/admin/v1/discovery", None),
        (
            "POST",
            "/admin/v1/tasks/unknown/retry-publication",
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/admin/v1/tasks/unknown/authorize-workflow-publication",
            Some(serde_json::json!({
                "attempt_id": "unknown",
                "attempt_record_sha256": "0".repeat(64),
                "reason": "reviewed",
                "actor": "maintainer"
            })),
        ),
        (
            "POST",
            "/admin/v1/discovery/scan",
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/admin/v1/discovery/pr_unknown/resume",
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/admin/v1/discovery/pr_unknown/pause",
            Some(serde_json::json!({})),
        ),
    ] {
        assert_eq!(admin(&base, method, path, body, "worker-secret").0, 403);
    }
    let (status, scan) = admin(
        &base,
        "POST",
        "/admin/v1/discovery/scan",
        Some(serde_json::json!({})),
        "admin-secret",
    );
    assert_eq!(status, 200);
    assert_eq!(scan["queued"], serde_json::json!([]));
    assert_eq!(
        admin(&base, "GET", "/admin/v1/discovery", None, "admin-secret").1,
        serde_json::json!([])
    );

    // Worker token cannot use the admin API; admin token cannot lease.
    let (status, _) = admin(
        &base,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project": "roles", "instructions": "x"})),
        "worker-secret",
    );
    assert_eq!(status, 403);
    let (status, _) = admin(&base, "GET", "/admin/v1/tasks", None, "nope");
    assert_eq!(status, 403);
    let lease = serde_json::json!({"worker_pool": "personal", "worker_id": "w1", "supported_job_schema_versions": [1, 2], "harnesses": [{"name": "fake", "version": "fake-1"}]});
    let (status, _) = admin(
        &base,
        "POST",
        "/worker/v1/lease",
        Some(lease.clone()),
        "admin-secret",
    );
    assert_eq!(status, 403);
    // A wrong-token daemon fails its first call.
    let d = daemon(&w, "wrong");
    assert!(
        tokio::task::spawn_blocking(move || d.run_once())
            .await
            .unwrap()
            .is_err()
    );

    // A leased worker cannot download an artifact its job does not reference.
    let (status, _) = admin(
        &base,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project": "roles", "instructions": "x"})),
        "admin-secret",
    );
    assert_eq!(status, 200);
    // Reject outdated workers before leasing anything, including after a controller upgrade.
    for versions in [serde_json::json!([1]), serde_json::json!([2])] {
        let mut incompatible = lease.clone();
        incompatible["supported_job_schema_versions"] = versions;
        let (status, _) = admin(
            &base,
            "POST",
            "/worker/v1/lease",
            Some(incompatible),
            "worker-secret",
        );
        assert_eq!(status, 400);
    }
    let (status, leased) = admin(
        &base,
        "POST",
        "/worker/v1/lease",
        Some(lease),
        "worker-secret",
    );
    assert_eq!(status, 200, "{leased}");
    let attempt_id = leased["attempt_id"].as_str().unwrap();
    let (status, _) = admin(
        &base,
        "GET",
        &format!(
            "/worker/v1/attempts/{attempt_id}/artifacts/{}",
            "0".repeat(64)
        ),
        None,
        "worker-secret",
    );
    assert_eq!(status, 404);
    let snapshot = leased["job"]["repositories"][0]["snapshot_bundle"]["sha256"]
        .as_str()
        .unwrap();
    let (status, _) = admin(
        &base,
        "GET",
        &format!("/worker/v1/attempts/{attempt_id}/artifacts/{snapshot}"),
        None,
        "worker-secret",
    );
    assert_eq!(status, 200);
    // Unauthenticated healthz still works.
    let (status, body) = admin(&base, "GET", "/healthz", None, "");
    assert_eq!((status, body.as_str().unwrap()), (200, "ok"));
}

fn browser_request(
    base: &str,
    path: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    form: Option<&[(&str, &str)]>,
) -> (u16, ureq::http::HeaderMap, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut response = if let Some(form) = form {
        let mut request = agent.post(format!("{base}{path}"));
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        request.send_form(form.iter().copied()).unwrap()
    } else {
        let mut request = agent.get(format!("{base}{path}"));
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        request.call().unwrap()
    };
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.body_mut().read_to_string().unwrap();
    (status, headers, body)
}

const UI_PASSWORD: &str = "a-separate-ui-password-for-tests";

fn test_ui_hash() -> String {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| UiConfig::hash_password(UI_PASSWORD).unwrap())
        .clone()
}

fn test_ui_user(
    id: &str,
    display_name: &str,
    instance_admin: bool,
    grants: BTreeMap<String, UiGrant>,
) -> UiUser {
    UiUser {
        id: id.into(),
        display_name: display_name.into(),
        password_hash: test_ui_hash(),
        disabled: false,
        instance_admin,
        grants,
        revision: 1,
        password_change_required: false,
        managed_by_config: false,
        timezone: "UTC".into(),
        timezone_initialized: true,
    }
}

fn login_browser(base: &str) -> String {
    let (status, headers, _) = browser_request(
        base,
        "/login",
        None,
        Some(base),
        Some(&[("username", "test-admin"), ("password", UI_PASSWORD)]),
    );
    assert_eq!(status, 303);
    headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

fn login_named_browser(base: &str, username: &str, password: &str) -> String {
    let (status, headers, _) = browser_request(
        base,
        "/login",
        None,
        Some(base),
        Some(&[("username", username), ("password", password)]),
    );
    assert_eq!(status, 303);
    headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn page_csrf(page: &str) -> String {
    page.split("name=\"csrf\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_is_disabled_without_explicit_configuration() {
    let w = world().await;
    for path in ["/", "/login", "/tasks", "/projects", "/assets/app.js"] {
        assert_eq!(
            browser_request(&w.base_url, path, None, None, None).0,
            404,
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn account_timezone_is_validated_saved_and_used_without_revoking_session() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let cookie = login_browser(&w.base_url);
    let (_, _, account) = browser_request(&w.base_url, "/account", Some(&cookie), None, None);
    assert!(account.contains("id=\"account-timezone\""));
    assert!(account.contains("class=\"timezone-control\""));
    assert!(account.contains("id=\"browser-timezone\" type=\"button\" disabled"));
    assert!(account.contains("data-timezone=\"UTC\""));
    let csrf = page_csrf(&account);
    assert_eq!(
        browser_request(
            &w.base_url,
            "/account/timezone",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", "wrong"), ("timezone", "Asia/Tokyo")]),
        )
        .0,
        403
    );
    let (status, _, invalid) = browser_request(
        &w.base_url,
        "/account/timezone",
        Some(&cookie),
        Some(&w.base_url),
        Some(&[("csrf", &csrf), ("timezone", "Mars/Phobos")]),
    );
    assert_eq!(status, 400);
    assert!(invalid.contains("valid IANA time zone"));
    assert_eq!(
        browser_request(
            &w.base_url,
            "/account/timezone",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", &csrf), ("timezone", "Asia/Tokyo")]),
        )
        .0,
        303
    );
    let (_, _, account) = browser_request(&w.base_url, "/account", Some(&cookie), None, None);
    assert!(account.contains("data-timezone=\"Asia/Tokyo\""));
    assert!(account.contains("<option value=\"Asia/Tokyo\" selected>Asia/Tokyo</option>"));
    assert!(account.contains("data-timezone-initialized=\"true\""));
    let (status, _) = admin(
        &w.base_url,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project": "roles", "instructions": "Check local time"})),
        "admin-secret",
    );
    assert_eq!(status, 200);
    let (_, _, tasks) = browser_request(&w.base_url, "/tasks", Some(&cookie), None, None);
    assert!(tasks.contains("data-relative-time"));
    assert!(tasks.contains("(Asia/Tokyo)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn navigation_counts_follow_authenticated_pages_and_project_grants() {
    use artidezi_store::model::{TaskState, TrackedPullRequest};

    let mut config = artidezi_config::parse(CONFIG).unwrap();
    let mut hidden = config.projects["roles"].clone();
    hidden.description = "Hidden project".into();
    config.projects.insert("other".into(), hidden);
    let w = world_configured_with(
        Some(UiConfig::new(false)),
        Duration::from_secs(60),
        config,
        |_| {},
    )
    .await;
    {
        let o = w.state.orchestrator.lock().unwrap();
        for user in [
            test_ui_user("owner", "Owner", true, BTreeMap::new()),
            test_ui_user(
                "reader",
                "Reader",
                false,
                BTreeMap::from([("roles".into(), UiGrant::Read)]),
            ),
        ] {
            o.db.create_ui_user(&user).unwrap();
        }
    }
    let mut visible_number = 0;
    let mut visible_id = String::new();
    for project in ["roles", "other"] {
        let (status, task) = admin(
            &w.base_url,
            "POST",
            "/admin/v1/tasks",
            Some(serde_json::json!({"project": project, "instructions": "Count this task"})),
            "admin-secret",
        );
        assert_eq!(status, 200);
        let id = task["id"].as_str().unwrap();
        if project == "roles" {
            visible_number = task["number"].as_i64().unwrap();
            visible_id = id.to_owned();
        }
        let mut o = w.state.orchestrator.lock().unwrap();
        o.db.set_task_state(id, TaskState::NeedsHuman, "test")
            .unwrap();
        o.db.save_pull_request_source(&TrackedPullRequest {
            id: format!("pr_{project}"),
            project: project.into(),
            repository_id: "role".into(),
            forge: "fake".into(),
            remote: "org/role".into(),
            number: 1,
            url: "https://example.invalid/pull/1".into(),
            title: "Update dependency".into(),
            closed: false,
            handler_id: "updates".into(),
            observed_head: "a".repeat(40),
            head_observed_at: chrono::Utc::now(),
            last_task_id: None,
            runs: 1,
            paused: false,
            status: "Evaluated".into(),
            follow_up: None,
        })
        .unwrap();
    }
    let event_id = {
        let o = w.state.orchestrator.lock().unwrap();
        o.db.list_events(&visible_id).unwrap()[0].id
    };
    for (username, expected) in [("owner", 2), ("reader", 1)] {
        let cookie = login_named_browser(&w.base_url, username, UI_PASSWORD);
        for path in [
            "/account".to_owned(),
            "/tasks".to_owned(),
            format!("/tasks/{visible_number}"),
            format!("/tasks/{visible_number}/events/{event_id}"),
            "/incoming-changes".to_owned(),
            "/projects".to_owned(),
            "/agent-profiles".to_owned(),
            "/workers".to_owned(),
        ] {
            let (status, _, page) = browser_request(&w.base_url, &path, Some(&cookie), None, None);
            assert_eq!(status, 200, "{username} {path}");
            for title in ["Tasks needing attention or review", "Open incoming changes"] {
                assert!(
                    page.contains(&format!("title=\"{title}\">{expected}</span>")),
                    "{username} {path}: {title}"
                );
            }
        }
        if username == "owner" {
            let (_, _, page) =
                browser_request(&w.base_url, "/tasks/new", Some(&cookie), None, None);
            assert!(page.contains("title=\"Tasks needing attention or review\">2</span>"));
            assert!(page.contains("title=\"Open incoming changes\">2</span>"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn named_ui_users_are_project_scoped_and_readers_cannot_mutate() {
    let mut config = artidezi_config::parse(CONFIG).unwrap();
    let mut other = config.projects["roles"].clone();
    other.description = "Hidden project".into();
    other.repositories.get_mut("role").unwrap().remote = "org/other".into();
    config.projects.insert("other".into(), other);
    let password = UI_PASSWORD;
    let ui = UiConfig::new(false);
    let restart_config = config.clone();
    let w = world_configured_with(Some(ui), Duration::from_secs(60), config, |_| {}).await;
    {
        let o = w.state.orchestrator.lock().unwrap();
        for user in [
            test_ui_user("owner", "Owner", true, BTreeMap::new()),
            test_ui_user(
                "reader",
                "Reader",
                false,
                BTreeMap::from([("roles".into(), UiGrant::Read)]),
            ),
            test_ui_user(
                "writer",
                "Writer",
                false,
                BTreeMap::from([("roles".into(), UiGrant::Write)]),
            ),
        ] {
            o.db.create_ui_user(&user).unwrap();
        }
    }
    let base = &w.base_url;
    let (_, role_task) = admin(
        base,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project":"roles","instructions":"Visible role task"})),
        "admin-secret",
    );
    let (_, other_task) = admin(
        base,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project":"other","instructions":"Hidden project task"})),
        "admin-secret",
    );
    let role_number = role_task["number"].as_i64().unwrap();
    let other_number = other_task["number"].as_i64().unwrap();
    let reader = login_named_browser(base, "reader", password);
    let (code, _, account) = browser_request(base, "/account", Some(&reader), None, None);
    assert_eq!(code, 200);
    assert!(account.contains("Reader"));
    assert!(account.contains("roles</code>: Read"));
    assert!(account.contains("title=\"Tasks needing attention or review\""));
    assert!(account.contains("title=\"Open incoming changes\""));
    let (code, _, page) = browser_request(base, "/tasks", Some(&reader), None, None);
    assert_eq!(code, 200);
    assert!(page.contains("Visible role task"));
    assert!(!page.contains("Hidden project task"));
    assert!(!page.contains("New task"));
    let (_, _, projects) = browser_request(base, "/projects", Some(&reader), None, None);
    assert!(projects.contains("A role."));
    assert!(!projects.contains("Hidden project"));
    let csrf = page_csrf(&page);
    let (_, _, detail) = browser_request(
        base,
        &format!("/tasks/{role_number}"),
        Some(&reader),
        None,
        None,
    );
    assert!(!detail.contains("Request a revision"));
    assert!(!detail.contains("Close task"));
    for path in [
        format!("/tasks/{other_number}"),
        format!("/tasks/{other_number}/events/1"),
        format!("/tasks/{other_number}/attempts/unknown/logs/unknown"),
    ] {
        assert_eq!(
            browser_request(base, &path, Some(&reader), None, None).0,
            404
        );
    }
    let hidden = browser_request(
        base,
        &format!("/tasks/{other_number}"),
        Some(&reader),
        None,
        None,
    )
    .2;
    let missing = browser_request(base, "/tasks/999999", Some(&reader), None, None).2;
    assert_eq!(hidden, missing);
    assert_eq!(
        browser_request(base, "/workers", Some(&reader), None, None).0,
        200
    );
    assert_eq!(
        browser_request(base, "/tasks/new", Some(&reader), None, None).0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/tasks",
            Some(&reader),
            Some(base),
            Some(&[
                ("csrf", &csrf),
                ("project", "roles"),
                ("instructions", "No")
            ])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            &format!("/tasks/{role_number}/close"),
            Some(&reader),
            Some(base),
            Some(&[("csrf", &csrf)])
        )
        .0,
        403
    );
    let writer = login_named_browser(base, "writer", password);
    let csrf = page_csrf(&browser_request(base, "/tasks", Some(&writer), None, None).2);
    assert_eq!(
        browser_request(
            base,
            "/tasks",
            Some(&writer),
            Some(base),
            Some(&[
                ("csrf", &csrf),
                ("project", "other"),
                ("instructions", "No")
            ])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/incoming-changes/scan",
            Some(&writer),
            Some(base),
            Some(&[("csrf", &csrf)])
        )
        .0,
        403
    );
    let (code, headers, _) = browser_request(
        base,
        "/tasks",
        Some(&writer),
        Some(base),
        Some(&[
            ("csrf", &csrf),
            ("project", "roles"),
            ("instructions", "Writer created task"),
        ]),
    );
    assert_eq!(code, 303);
    let number = headers["location"]
        .to_str()
        .unwrap()
        .split('/')
        .nth(2)
        .unwrap()
        .split('?')
        .next()
        .unwrap();
    let (_, all_tasks) = admin(base, "GET", "/admin/v1/tasks", None, "admin-secret");
    let created = all_tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["number"].as_i64() == number.parse().ok())
        .unwrap();
    assert_eq!(created["created_by"], "user:writer (Writer)");
    let owner = login_named_browser(base, "owner", password);
    assert_eq!(
        browser_request(base, "/workers", Some(&owner), None, None).0,
        200
    );
    assert_eq!(
        browser_request(
            base,
            &format!("/tasks/{other_number}"),
            Some(&owner),
            None,
            None
        )
        .0,
        200
    );
    {
        let mut o = w.state.orchestrator.lock().unwrap();
        let mut writer_user = o.db.get_ui_user("writer").unwrap().unwrap();
        writer_user.grants.insert("roles".into(), UiGrant::Read);
        assert!(o.db.update_ui_user(&writer_user).unwrap());
    }
    assert_eq!(
        browser_request(base, "/tasks", Some(&writer), None, None).0,
        200
    );
    assert_eq!(
        browser_request(
            base,
            "/tasks",
            Some(&writer),
            Some(base),
            Some(&[
                ("csrf", &csrf),
                ("project", "roles"),
                ("instructions", "No longer a writer"),
            ])
        )
        .0,
        403
    );
    {
        let mut o = w.state.orchestrator.lock().unwrap();
        let mut reader_user = o.db.get_ui_user("reader").unwrap().unwrap();
        reader_user.disabled = true;
        assert!(o.db.update_ui_user(&reader_user).unwrap());
    }
    assert_eq!(
        browser_request(base, "/tasks", Some(&reader), None, None).0,
        303
    );
    let restarted = reopen_with_ui(&w, restart_config, UiConfig::new(false)).await;
    assert_eq!(
        browser_request(&restarted, "/tasks", Some(&reader), None, None).0,
        303
    );
    assert_eq!(
        browser_request(
            &restarted,
            "/login",
            None,
            Some(&restarted),
            Some(&[("username", "reader"), ("password", password)])
        )
        .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_assets_are_content_addressed_without_caching_private_pages_or_errors() {
    use sha2::{Digest, Sha256};

    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let base = &w.base_url;
    let (_, headers, login) = browser_request(base, "/login", None, None, None);
    assert_eq!(headers["cache-control"], "no-store");
    for (attribute, extension, content_type) in [
        ("href", "css", "text/css; charset=utf-8"),
        ("src", "js", "text/javascript; charset=utf-8"),
        ("href", "svg", "image/svg+xml"),
    ] {
        let path = login
            .split(&format!("{attribute}=\"/assets/"))
            .skip(1)
            .filter_map(|part| part.split('"').next())
            .find(|path| path.ends_with(&format!(".{extension}")))
            .unwrap();
        let path = format!("/assets/{path}");
        if extension == "svg" {
            assert!(login.contains(&format!(
                r#"<link rel="icon" type="image/svg+xml" href="{path}">"#
            )));
            assert!(login.contains(&format!(r#"<img class="brand-mark" src="{path}""#)));
        }
        let (code, headers, body) = browser_request(base, &path, None, None, None);
        assert_eq!(code, 200);
        assert_eq!(headers["content-type"], content_type);
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        assert_eq!(path, format!("/assets/app.{hash}.{extension}"));
        let (code, headers, unversioned) =
            browser_request(base, &format!("/assets/app.{extension}"), None, None, None);
        assert_eq!(code, 200);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(body, unversioned);
        let old_hash = if hash.starts_with('0') { "1" } else { "0" }.repeat(64);
        let (code, headers, _) = browser_request(
            base,
            &format!("/assets/app.{old_hash}.{extension}"),
            None,
            None,
            None,
        );
        assert_eq!(code, 307);
        assert_eq!(headers["location"], path);
        assert_eq!(headers["cache-control"], "no-store");
        let (code, headers, _) = browser_request(base, &path, None, None, Some(&[]));
        assert_eq!(code, 405);
        assert_eq!(headers["cache-control"], "no-store");
    }
    let cookie = login_browser(base);
    for (path, code) in [
        ("/tasks", 200),
        ("/tasks/999999", 404),
        ("/assets/app.invalid.css", 404),
        ("/assets/missing.js", 404),
    ] {
        let (actual, headers, _) = browser_request(base, path, Some(&cookie), None, None);
        assert_eq!(actual, code, "{path}");
        assert_eq!(headers["cache-control"], "no-store", "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_long_instructions_keep_complete_safe_markdown_without_javascript() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let instructions = format!(
        "# Requested work\n\n{}\n**Final requirement** <script>unsafe()</script>",
        "- Preserve the public interface.\n".repeat(30)
    );
    let task = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .create_manual_task(
            "roles",
            &instructions,
            "admin",
            &artidezi_config::resolve::TaskSelection::default(),
        )
        .unwrap();
    let cookie = login_browser(&w.base_url);
    let (_, headers, page) = browser_request(
        &w.base_url,
        &format!("/tasks/{}", task.number),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(headers["cache-control"], "no-store");
    assert!(page.contains(r#"id="instructions-preview" hidden>"#));
    let content = page.split(r#"id="instructions-content">"#).nth(1).unwrap();
    assert!(content.contains("<strong>Final requirement</strong>"));
    assert!(content.contains("&lt;script&gt;unsafe()&lt;/script&gt;"));
    assert!(!page.contains("<script>unsafe()"));
    assert!(page.contains(r#"aria-controls="instructions-content instructions-preview" aria-expanded="true" hidden>Show less</button>"#));
    assert!(!page.contains(&format!(r#"href="/tasks/{}/instructions""#, task.number)));
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_authentication_is_separate_and_csrf_protected() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let base = &w.base_url;
    for path in [
        "/tasks",
        "/tasks/new",
        "/projects",
        "/tasks/anything",
        "/tasks/anything/instructions",
        "/tasks/anything/events/1",
        "/incoming-changes",
        "/tasks/x/attempts/y/logs/z",
        "/tasks/x/attempts/y/patches/z",
    ] {
        let (code, headers, body) = browser_request(base, path, None, None, None);
        assert_eq!(code, 303, "{path}: {body}");
        assert_eq!(headers["location"], "/login");
        assert_eq!(headers["cache-control"], "no-store");
    }
    // Login CSRF is rejected before checking a correct password.
    assert_eq!(
        browser_request(
            base,
            "/login",
            None,
            None,
            Some(&[("username", "test-admin"), ("password", UI_PASSWORD)])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/login",
            None,
            Some("https://other.invalid"),
            Some(&[("username", "test-admin"), ("password", UI_PASSWORD)])
        )
        .0,
        403
    );
    for wrong in ["wrong", "admin-secret", "worker-secret"] {
        let (code, _, body) = browser_request(
            base,
            "/login",
            None,
            Some(base),
            Some(&[("password", wrong)]),
        );
        assert_eq!(code, 401);
        assert!(!body.contains(UI_PASSWORD));
    }
    let cookie = login_browser(base);
    assert!(!cookie.contains(UI_PASSWORD));
    assert_eq!(
        browser_request(base, "/admin/users", Some(&cookie), None, None).0,
        404
    );
    let (code, headers, body) = browser_request(base, "/tasks", Some(&cookie), None, None);
    assert_eq!(code, 200);
    // no-referrer makes Chromium send Origin: null on a form POST, breaking login/CSRF.
    assert_eq!(headers["referrer-policy"], "same-origin");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert!(!body.contains("admin-secret"));
    let csrf = page_csrf(&body);
    // A browser session cannot authorize the token-only admin or worker APIs.
    assert_eq!(
        browser_request(base, "/admin/v1/tasks", Some(&cookie), None, None).0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/tasks",
            Some(&cookie),
            Some(base),
            Some(&[
                ("csrf", "wrong"),
                ("project", "roles"),
                ("instructions", "x")
            ])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/tasks",
            Some(&cookie),
            Some("https://other.invalid"),
            Some(&[("csrf", &csrf), ("project", "roles"), ("instructions", "x")])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            base,
            "/logout",
            Some(&cookie),
            Some(base),
            Some(&[("csrf", &csrf)])
        )
        .0,
        303
    );
    assert_eq!(
        browser_request(base, "/tasks", Some(&cookie), None, None).0,
        303
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_uses_secure_cookies_and_throttles_bad_passwords() {
    let w = world_with_ui(Some(UiConfig::new(true))).await;
    let secure_origin = w.base_url.replace("http://", "https://");
    assert_eq!(
        browser_request(
            &w.base_url,
            "/login",
            None,
            Some(&w.base_url),
            Some(&[("username", "test-admin"), ("password", UI_PASSWORD)])
        )
        .0,
        403
    );
    let (code, headers, _) = browser_request(
        &w.base_url,
        "/login",
        None,
        Some(&secure_origin),
        Some(&[("username", "test-admin"), ("password", UI_PASSWORD)]),
    );
    assert_eq!(code, 303);
    let cookie = headers["set-cookie"].to_str().unwrap();
    for property in [
        "__Host-artidezi_session=",
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
        "Max-Age=2592000",
    ] {
        assert!(cookie.contains(property));
    }
    for _ in 0..20 {
        assert_eq!(
            browser_request(
                &w.base_url,
                "/login",
                None,
                Some(&secure_origin),
                Some(&[("password", "bad")])
            )
            .0,
            401
        );
    }
    assert_eq!(
        browser_request(
            &w.base_url,
            "/login",
            None,
            Some(&secure_origin),
            Some(&[("password", "bad")])
        )
        .0,
        429
    );
    assert_eq!(
        browser_request(
            &w.base_url,
            "/login",
            None,
            Some(&secure_origin),
            Some(&[("username", "other"), ("password", "bad")])
        )
        .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_sessions_survive_restart_with_csrf_and_honor_revocation() {
    async fn reopen(w: &World) -> String {
        let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
        forges.insert("fake".into(), Box::new(w.forge.clone()));
        let config = artidezi_config::parse(CONFIG).unwrap();
        // Reopen durable state with a fresh HTTP/router state, without copying any sessions.
        let state = Arc::new(AppState::new(
            Orchestrator::open(config, &w.root.join("data"), forges).unwrap(),
            Tokens {
                workers: Vec::new(),
                admin: "admin-secret".into(),
            },
            Duration::from_secs(60),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router_with_ui(state, Some(UiConfig::new(false)));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    {
        let mut o = w.state.orchestrator.lock().unwrap();
        let mut user = o.db.get_ui_user("test-admin").unwrap().unwrap();
        user.password_change_required = true;
        assert!(o.db.update_ui_user(&user).unwrap());
    }
    let cookie = login_browser(&w.base_url);
    let page = browser_request(&w.base_url, "/tasks", Some(&cookie), None, None).2;
    assert!(page.contains("password-notice"));
    let csrf = page_csrf(&page);
    let restarted = reopen(&w).await;
    let (code, _, page) = browser_request(&restarted, "/tasks", Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert_eq!(
        page_csrf(&page),
        csrf,
        "Existing forms still work after restart"
    );
    let tampered = format!("{}x", cookie);
    assert_eq!(
        browser_request(&restarted, "/tasks", Some(&tampered), None, None).0,
        303
    );
    assert_eq!(
        browser_request(
            &restarted,
            "/logout",
            Some(&cookie),
            Some(&restarted),
            Some(&[("csrf", &csrf)])
        )
        .0,
        303
    );
    let restarted = reopen(&w).await;
    assert_eq!(
        browser_request(&restarted, "/tasks", Some(&cookie), None, None).0,
        303,
        "Logout persists"
    );

    let cookie = login_browser(&restarted);
    let other_cookie = login_browser(&restarted);
    let csrf = page_csrf(&browser_request(&restarted, "/account", Some(&cookie), None, None).2);
    let new_password = "another-long-random-ui-test-password";
    let (code, _, failed_password_page) = browser_request(
        &restarted,
        "/account/password",
        Some(&cookie),
        Some(&restarted),
        Some(&[
            ("csrf", &csrf),
            ("current_password", "wrong"),
            ("new_password", new_password),
            ("confirm_password", new_password),
        ]),
    );
    assert_eq!(code, 401);
    assert!(failed_password_page.contains("title=\"Tasks needing attention or review\""));
    assert!(failed_password_page.contains("title=\"Open incoming changes\""));
    assert_eq!(
        browser_request(
            &restarted,
            "/account/password",
            Some(&cookie),
            Some(&restarted),
            Some(&[
                ("csrf", &csrf),
                ("current_password", UI_PASSWORD),
                ("new_password", new_password),
                ("confirm_password", "different"),
            ])
        )
        .0,
        400
    );
    assert_eq!(
        browser_request(
            &restarted,
            "/account/password",
            Some(&cookie),
            Some(&restarted),
            Some(&[
                ("csrf", &csrf),
                ("current_password", UI_PASSWORD),
                ("new_password", new_password),
                ("confirm_password", new_password),
            ])
        )
        .0,
        303
    );
    let rotated = reopen(&w).await;
    assert_eq!(
        browser_request(&rotated, "/tasks", Some(&cookie), None, None).0,
        303,
        "Password rotation revokes old sessions"
    );
    assert_eq!(
        browser_request(&rotated, "/tasks", Some(&other_cookie), None, None).0,
        303
    );
    assert_eq!(
        browser_request(
            &rotated,
            "/login",
            None,
            Some(&rotated),
            Some(&[("username", "test-admin"), ("password", UI_PASSWORD)])
        )
        .0,
        401
    );
    let (code, _, _) = browser_request(
        &rotated,
        "/login",
        None,
        Some(&rotated),
        Some(&[("username", "test-admin"), ("password", new_password)]),
    );
    assert_eq!(code, 303);
    let fresh_cookie = login_named_browser(&rotated, "test-admin", new_password);
    assert!(
        !browser_request(&rotated, "/tasks", Some(&fresh_cookie), None, None)
            .2
            .contains("password-notice")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_creates_revises_and_closes_tasks_and_escapes_untrusted_text() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let base = &w.base_url;
    let cookie = login_browser(base);
    let (_, _, page) = browser_request(base, "/tasks/new", Some(&cookie), None, None);
    let csrf = page_csrf(&page);
    let instructions = "Fix <script>alert('unsafe')</script> & **document the result**";
    for (field, bad) in [
        ("project", "unknown"),
        ("agent_profile", "unknown"),
        ("delivery_mode", "direct_push_fast_forward"),
    ] {
        let mut fields = vec![
            ("csrf", csrf.as_str()),
            ("project", "roles"),
            ("instructions", instructions),
        ];
        fields.retain(|(key, _)| *key != field);
        fields.push((field, bad));
        let (code, _, page) =
            browser_request(base, "/tasks", Some(&cookie), Some(base), Some(&fields));
        assert_eq!(code, 400, "{page}");
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>alert"));
    }
    let (code, headers, _) = browser_request(
        base,
        "/tasks",
        Some(&cookie),
        Some(base),
        Some(&[
            ("csrf", &csrf),
            ("project", "roles"),
            ("instructions", instructions),
        ]),
    );
    assert_eq!(code, 303);
    let path = headers["location"]
        .to_str()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned();
    let number: i64 = path.strip_prefix("/tasks/").unwrap().parse().unwrap();
    let id = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .db
        .get_task_by_number(number)
        .unwrap()
        .id;
    let d = daemon(&w, "worker-secret");
    assert!(
        tokio::task::spawn_blocking(move || d.run_once())
            .await
            .unwrap()
            .unwrap()
    );
    let (code, _, page) = browser_request(base, &path, Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert!(page.contains("Ready for review"));
    assert!(page.contains("Recorded a change over HTTP."));
    assert!(page.contains("<strong>document the result</strong>"));
    assert!(page.contains("&lt;script&gt;"));
    assert!(!page.contains("<script>alert"));
    // Both evidence views are restricted to verified text artifacts of this task/attempt.
    let (attempt_id, patch_sha, log_sha, raw_patch, unrelated_task) = {
        let mut o = w.state.orchestrator.lock().unwrap();
        let mut other = o.db.get_task(&id).unwrap();
        other.id = "tsk_unrelated_patch_test".into();
        o.db.insert_task(&other).unwrap();
        let attempt = o.db.list_attempts(&id).unwrap().remove(0);
        let record: artidezi_contract::attempt::AttemptRecord = o
            .artifacts
            .get_json(attempt.attempt_record.as_ref().unwrap())
            .unwrap();
        let patch = record
            .artifacts
            .iter()
            .find(|a| a.name.ends_with(".patch"))
            .unwrap();
        let log = record
            .artifacts
            .iter()
            .find(|a| a.name.ends_with("stdout.log"))
            .unwrap();
        let raw = o
            .artifacts
            .get(&artidezi_contract::common::ArtifactRef {
                sha256: patch.sha256.clone(),
                size: patch.size,
            })
            .unwrap();
        (
            attempt.id,
            patch.sha256.clone(),
            log.sha256.clone(),
            raw,
            other.id,
        )
    };
    let patch_path = format!("{path}/attempts/{attempt_id}/patches/{patch_sha}");
    assert!(page.contains(&patch_path));
    let (code, headers, preview) = browser_request(base, &patch_path, Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(preview.contains("diff-added"));
    assert!(preview.contains("CHANGES.md</h2>"));
    assert!(preview.contains(&format!("?attempt=1#attempt-{attempt_id}")));
    assert!(preview.contains("Attempt #1</a>"));
    assert!(preview.contains("/tasks?project=roles"));
    assert!(!preview.contains("Back to attempt"));
    let raw_path = format!("{path}/attempts/{attempt_id}/logs/{patch_sha}");
    assert!(preview.contains(&raw_path));
    let (code, headers, raw) = browser_request(base, &raw_path, Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(raw.as_bytes(), raw_patch);
    for invalid in [
        format!("{path}/attempts/{attempt_id}/patches/{log_sha}"),
        format!("{path}/attempts/{attempt_id}/patches/{}", "0".repeat(64)),
        format!("{path}/attempts/unrelated/patches/{patch_sha}"),
        format!("/tasks/{unrelated_task}/attempts/{attempt_id}/patches/{patch_sha}"),
    ] {
        assert_eq!(
            browser_request(base, &invalid, Some(&cookie), None, None).0,
            404
        );
    }
    assert_eq!(
        browser_request(
            base,
            &format!("{path}/revisions"),
            Some(&cookie),
            Some(base),
            Some(&[("csrf", &csrf), ("feedback", "Please revise the note")])
        )
        .0,
        303
    );
    // A second request races with the queued revision. Preserve the user's draft
    // and escape it in the error response instead of losing it on a generic page.
    let (code, _, page) = browser_request(
        base,
        &format!("{path}/revisions"),
        Some(&cookie),
        Some(base),
        Some(&[
            ("csrf", &csrf),
            ("feedback", "Keep <script>this draft</script>"),
        ]),
    );
    assert!((400..500).contains(&code), "{page}");
    assert!(page.contains("Keep &lt;script&gt;this draft&lt;/script&gt;"));
    assert!(!page.contains("<script>this draft"));
    let d = daemon(&w, "worker-secret");
    assert!(
        tokio::task::spawn_blocking(move || d.run_once())
            .await
            .unwrap()
            .unwrap()
    );
    let (_, _, page) = browser_request(base, &path, Some(&cookie), None, None);
    assert!(page.contains("Attempt 2"));
    assert!(page.contains("Please revise the note"));
    assert!(!page.contains("<details"));
    assert_eq!(page.matches("class=\"panel attempt\"").count(), 1);
    let (_, _, previous) = browser_request(
        base,
        &format!("{path}?attempt=1"),
        Some(&cookie),
        None,
        None,
    );
    assert!(previous.contains("Attempt 1</h2>"));
    assert!(!previous.contains("id=\"feedback\""));
    assert_eq!(
        browser_request(
            base,
            &format!("{path}?attempt=9999"),
            Some(&cookie),
            None,
            None
        )
        .0,
        404
    );
    let (code, headers, _) = browser_request(
        base,
        &format!("{path}/instructions"),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(code, 303);
    assert_eq!(headers["location"], format!("{path}#instructions-content"));
    let (_, _, full) = browser_request(base, &path, Some(&cookie), None, None);
    assert!(full.contains("&lt;script&gt;"));
    assert!(full.contains("<strong>document the result</strong>"));
    assert!(!full.contains("<script>alert"));
    let event_id = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .db
        .list_events(&id)
        .unwrap()[0]
        .id;
    let (code, _, event) = browser_request(
        base,
        &format!("{path}/events/{event_id}"),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(code, 200);
    assert!(event.contains("event-payload"));
    assert_eq!(
        browser_request(
            base,
            &format!("{path}/events/999999"),
            Some(&cookie),
            None,
            None
        )
        .0,
        404
    );

    let task = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .db
        .get_task(&id)
        .unwrap();
    assert_eq!(task.created_by, "user:test-admin (Test admin)");
    // Without JavaScript the first POST is a confirmation page, not a mutation.
    let (status, _, confirmation) = browser_request(
        base,
        &format!("{path}/close"),
        Some(&cookie),
        Some(base),
        Some(&[("csrf", &csrf)]),
    );
    assert_eq!(status, 200);
    assert!(confirmation.contains("Close this task?"));
    assert_eq!(
        w.state
            .orchestrator
            .lock()
            .unwrap()
            .db
            .get_task(&id)
            .unwrap()
            .state,
        task.state
    );
    assert_eq!(
        browser_request(
            base,
            &format!("{path}/close"),
            Some(&cookie),
            Some(base),
            Some(&[("csrf", &csrf), ("confirmed", "true")])
        )
        .0,
        303
    );
    let (_, _, page) = browser_request(base, &path, Some(&cookie), None, None);
    assert!(page.contains(r#"class="status closed""#));
    assert!(!page.contains("id=\"feedback\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_search_handles_many_projects_and_escaped_queries_without_javascript() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        let template = o.config.projects["roles"].clone();
        for number in 0..40 {
            let mut project = template.clone();
            project.description = format!("Maintenance for component {number}");
            project.repositories.get_mut("role").unwrap().remote =
                format!("org/library-{number:02}");
            o.config
                .projects
                .insert(format!("application-{number:02}"), project);
        }
    })
    .await;
    let cookie = login_browser(&w.base_url);
    for (query, expected) in [
        ("", 41),
        ("?q=APPLICATION-17", 1),
        ("?q=org%2Flibrary-17", 1),
        ("?q=not-a-project", 0),
    ] {
        let (code, _, body) = browser_request(
            &w.base_url,
            &format!("/projects{query}"),
            Some(&cookie),
            None,
            None,
        );
        assert_eq!(code, 200);
        assert_eq!(
            body.matches("class=\"panel project-card\"").count(),
            expected
        );
        assert!(body.contains(&format!("{expected} of 41 projects")));
    }
    for path in [
        "/projects?q=%3Cscript%3Ealert%281%29%3C%2Fscript%3E",
        "/tasks?q=%3Cscript%3Ealert%281%29%3C%2Fscript%3E&filter=review",
    ] {
        let (code, _, body) = browser_request(&w.base_url, path, Some(&cookie), None, None);
        assert_eq!(code, 200);
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!body.contains("<script>alert"));
    }
    let (code, _, workers) = browser_request(&w.base_url, "/workers", Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert!(workers.contains("Configured worker pools"));
    assert!(workers.contains("Worker pool"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_project_filters_paginate_and_task_numbers_keep_old_links() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        let definition = o.config.projects["roles"].clone();
        o.config.projects.insert("roles-extra".into(), definition);
        let original = o
            .create_manual_task(
                "roles",
                "needle: first task",
                "admin",
                &artidezi_config::resolve::TaskSelection::default(),
            )
            .unwrap();
        o.db.set_task_state(
            &original.id,
            artidezi_store::model::TaskState::AwaitingReview,
            "test",
        )
        .unwrap();
        for index in 1..57 {
            let mut task = original.clone();
            task.id = artidezi_store::new_id("tsk");
            task.number = 0;
            task.instructions = format!("needle: task {index}");
            task.state = artidezi_store::model::TaskState::AwaitingReview;
            if index == 55 {
                task.state = artidezi_store::model::TaskState::NeedsHuman;
            }
            if index == 56 {
                task.project = "roles-extra".into();
                task.instructions = "Unrelated project task".into();
            }
            o.db.insert_task(&task).unwrap();
        }
    })
    .await;
    let original = w
        .state
        .orchestrator
        .lock()
        .unwrap()
        .db
        .get_task_by_number(1)
        .unwrap();
    let cookie = login_browser(&w.base_url);
    for (page, count) in [(1, 50), (2, 5)] {
        let (code, _, body) = browser_request(
            &w.base_url,
            &format!("/tasks?project=roles&filter=review&q=needle&page={page}"),
            Some(&cookie),
            None,
            None,
        );
        assert_eq!(code, 200);
        assert_eq!(body.matches("class=\"task-row row-tone-").count(), count);
        assert!(body.contains(&format!("Page {page} of 2 · 55 tasks")));
        assert!(body.contains("project=roles&amp;filter=review&amp;q=needle&amp;page="));
        assert!(body.contains(r#"<option value="roles" selected>roles</option>"#));
        assert!(body.contains(r#"class="task-project" href="/tasks?project=roles""#));
        assert!(!body.contains("Unrelated project task"));
    }
    let (_, _, attention) = browser_request(
        &w.base_url,
        "/tasks?project=roles&filter=attention&q=needle",
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(attention.matches("class=\"task-row row-tone-").count(), 1);
    let (_, _, projects) = browser_request(&w.base_url, "/projects", Some(&cookie), None, None);
    assert!(projects.contains(r#"href="/tasks?project=roles""#));
    assert!(projects.contains("56 tasks</a>"));
    let (_, _, old) = browser_request(
        &w.base_url,
        &format!("/tasks/{}", original.id),
        Some(&cookie),
        None,
        None,
    );
    let (code, _, numeric) = browser_request(
        &w.base_url,
        &format!("/tasks/{}", original.number),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(code, 200);
    assert_eq!(old, numeric);
    assert!(numeric.contains(&format!(
        r#"aria-current="page">Task #{}</span>"#,
        original.number
    )));
    assert!(numeric.contains(&format!("/tasks/{}/revisions", original.id)));
    assert_eq!(
        browser_request(&w.base_url, "/tasks/99999", Some(&cookie), None, None).0,
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_distinguishes_automatic_ci_waiting_from_worker_activity_and_human_holds() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        let original = o
            .create_manual_task(
                "roles",
                "Ordinary worker task",
                "admin",
                &artidezi_config::resolve::TaskSelection::default(),
            )
            .unwrap();
        let mut task = original.clone();
        task.id = artidezi_store::new_id("tsk");
        task.number = 0;
        task.kind = artidezi_store::model::TaskKind::PullRequest;
        task.instructions = "An accepted update waiting for checks".into();
        o.db.insert_task(&task).unwrap();
        o.db.wait_for_source_ci(&task.id, chrono::Utc::now() + chrono::Duration::minutes(10))
            .unwrap();
    })
    .await;
    let cookie = login_browser(&w.base_url);
    let (code, _, page) = browser_request(
        &w.base_url,
        "/tasks?project=roles&filter=waiting",
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(code, 200);
    assert_eq!(page.matches("class=\"task-row row-tone-").count(), 1);
    assert!(page.contains("An accepted update waiting for checks"));
    assert!(!page.contains("Ordinary worker task"));
    assert!(page.contains("status waiting_for_ci"));
    let (code, _, page) = browser_request(&w.base_url, "/tasks/2", Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert!(page.contains("Waiting for CI"));
    assert!(page.contains("Delivery continues automatically"));
    assert!(!page.contains("Retry publication"));
    assert!(!page.contains("Evaluate again"));
    assert!(page.contains("Close task"));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_profiles_require_login_and_show_only_selected_configuration() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        let optional = o.config.agent_profiles.get("fake").unwrap().clone();
        o.config
            .agent_profiles
            .insert("fake-optional".into(), optional);
        o.config
            .projects
            .get_mut("roles")
            .unwrap()
            .allowed_agent_profiles
            .push("fake-optional".into());
        o.config
            .agent_profiles
            .get_mut("fake")
            .unwrap()
            .model_settings
            .insert(
                "private_setting".into(),
                serde_json::json!("must-not-render"),
            );
    })
    .await;
    assert_eq!(
        browser_request(&w.base_url, "/agent-profiles", None, None, None).0,
        303
    );
    let cookie = login_browser(&w.base_url);
    let (code, _, page) =
        browser_request(&w.base_url, "/agent-profiles", Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert!(page.contains("Agent profiles"));
    assert!(page.contains("No model (scripted)"));
    assert!(page.contains("Worker pool"));
    assert!(!page.contains("must-not-render"));
    assert!(!page.contains("credential_ref"));
    let default_card = page
        .split(r#"<article class="panel agent-card" id="fake">"#)
        .nth(1)
        .unwrap()
        .split("</article>")
        .next()
        .unwrap();
    assert!(default_card.contains("Default for 1 project:"));
    assert!(default_card.contains(r#"Also available for:</span><span class="muted">None</span>"#));
    let optional_card = page
        .split(r#"<article class="panel agent-card" id="fake-optional">"#)
        .nth(1)
        .unwrap()
        .split("</article>")
        .next()
        .unwrap();
    assert!(optional_card.contains("Also available for 1 project:"));
    assert!(optional_card.contains(r#"Default for:</span><span class="muted">None</span>"#));
    let (_, _, projects) = browser_request(&w.base_url, "/projects", Some(&cookie), None, None);
    assert!(projects.contains("/agent-profiles#fake"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_incoming_changes_filter_projects_and_paginate_without_scanning() {
    use artidezi_config::model::PullRequestDiscovery;
    use artidezi_store::model::TrackedPullRequest;
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        o.config.forges.get_mut("fake").unwrap().web_base_url =
            Some("http://127.0.0.1:3081".into());
        o.config.projects.get_mut("roles").unwrap().pull_requests = Some(PullRequestDiscovery {
            poll_interval_seconds: 600,
            settle_seconds: 0,
            max_runs_per_pull_request: 3,
            max_active_tasks: 1,
            repositories: vec!["consumer".into()],
            ci_log_context: Default::default(),
            handlers: vec![],
        });
        let project = o.config.projects.get_mut("roles").unwrap();
        let mut consumer = project.repositories["role"].clone();
        consumer.remote = "org/library".into();
        project.repositories.insert("consumer".into(), consumer);
        for number in 1..=28 {
            let source = TrackedPullRequest {
                id: format!("pr_{number}"),
                project: "roles".into(),
                repository_id: "consumer".into(),
                forge: "fake".into(),
                remote: "org/library".into(),
                number,
                url: "http://internal.invalid/pull".into(),
                title: format!("Upgrade dependency <script>{number}</script>"),
                closed: number > 2,
                handler_id: "updates".into(),
                observed_head: "a".repeat(40),
                head_observed_at: chrono::Utc::now(),
                last_task_id: None,
                runs: 1,
                paused: number == 1,
                status: if number == 2 {
                    "Source PR is closed or merged".into()
                } else {
                    "This head has already been evaluated".into()
                },
                follow_up: None,
            };
            o.db.save_pull_request_source(&source).unwrap();
        }
        let mut other_project = o.config.projects["roles"].clone();
        other_project
            .repositories
            .get_mut("consumer")
            .unwrap()
            .remote = "org/application".into();
        o.config
            .projects
            .insert("application".into(), other_project);
        let mut other = o.db.get_pull_request_source("pr_1").unwrap();
        other.id = "pr_other".into();
        other.project = "application".into();
        other.remote = "org/application".into();
        other.paused = false;
        o.db.save_pull_request_source(&other).unwrap();
        other.id = "pr_retired".into();
        other.repository_id = "retired".into();
        other.remote = "org/retired".into();
        other.forge = "retired-forge".into();
        other.url = "https://retired.example/org/retired/pulls/1".into();
        o.db.save_pull_request_source(&other).unwrap();
    })
    .await;
    let cookie = login_browser(&w.base_url);
    let (_, _, all) = browser_request(&w.base_url, "/incoming-changes", Some(&cookie), None, None);
    assert_eq!(
        all.matches("class=\"task-row discovery-source row-tone-")
            .count(),
        2
    );
    assert!(all.contains("org/application"));
    assert!(all.contains(">#1</a>"));
    assert!(all.contains("class=\"source-repository\""));
    assert!(all.contains("class=\"source-title-group\">(<a class=\"source-title\""));
    assert!(all.contains("Dry run"));
    assert!(all.contains("Discovery off"));
    assert!(all.contains("href=\"https://retired.example/org/retired/pulls/1\""));
    assert!(!all.contains("/incoming-changes/pr_retired/resume"));
    assert!(!all.contains("/incoming-changes/pr_retired/pause"));
    {
        let o = w.state.orchestrator.lock().unwrap();
        let mut retired = o.db.get_pull_request_source("pr_retired").unwrap();
        retired.url = "javascript:alert(1)".into();
        o.db.save_pull_request_source(&retired).unwrap();
    }
    let (_, _, unlinked) = browser_request(
        &w.base_url,
        "/incoming-changes?project=application",
        Some(&cookie),
        None,
        None,
    );
    assert!(!unlinked.contains("href=\"javascript:"));
    assert!(!unlinked.contains("#/pulls/"));
    assert!(unlinked.contains("<span class=\"source-repository\">"));
    assert!(unlinked.contains("<span class=\"source-title-group\">(<span class=\"source-title\">"));
    assert!(all.contains("name=\"project\""));
    assert!(!all.contains("Shadow assessment"));
    let (_, _, projects) = browser_request(&w.base_url, "/projects", Some(&cookie), None, None);
    assert!(projects.contains("http://127.0.0.1:3081/org/role"));
    assert!(projects.contains("1 incoming change"));
    let path = "/incoming-changes?project=roles";
    let (code, _, open) = browser_request(&w.base_url, path, Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert_eq!(
        open.matches("class=\"task-row discovery-source row-tone-")
            .count(),
        0
    );
    assert!(!open.contains("org/library"));
    assert!(!open.contains("consumer #"));
    assert!(!open.contains("org/application"));
    assert!(open.contains("value=\"roles\" selected"));
    let (_, _, paused) = browser_request(
        &w.base_url,
        "/incoming-changes?filter=paused",
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(
        paused
            .matches("class=\"task-row discovery-source row-tone-")
            .count(),
        1
    );
    assert!(paused.contains("org/library"));
    assert!(paused.contains(">#1</a>"));
    assert!(paused.contains("http://127.0.0.1:3081/org/library/pull/1"));
    assert!(paused.contains("Upgrade dependency &lt;script&gt;1&lt;/script&gt;"));
    assert!(paused.contains("Resume evaluations"));
    let (_, _, no_match) = browser_request(
        &w.base_url,
        "/incoming-changes?project=missing",
        Some(&cookie),
        None,
        None,
    );
    assert!(!no_match.contains("class=\"task-row discovery-source row-tone-"));
    assert!(no_match.contains("Discovery is off"));
    assert!(!no_match.contains("Scan now"));
    let (_, _, history) = browser_request(
        &w.base_url,
        &format!("{path}&filter=history&q=dependency"),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(
        history
            .matches("class=\"task-row discovery-source row-tone-")
            .count(),
        25
    );
    assert!(history.contains("Page 1 of 2 · 27 changes"));
    assert!(history.contains("filter=history&amp;q=dependency&amp;page=2"));
    assert!(!history.contains("Pause evaluations"));
    let (_, _, second) = browser_request(
        &w.base_url,
        &format!("{path}&filter=history&q=dependency&page=2"),
        Some(&cookie),
        None,
        None,
    );
    assert_eq!(
        second
            .matches("class=\"task-row discovery-source row-tone-")
            .count(),
        2
    );
    assert!(second.contains("org/library"));
    assert!(second.contains(">#2</a>"));
    assert!(
        w.state
            .orchestrator
            .lock()
            .unwrap()
            .db
            .list_tasks(None)
            .unwrap()
            .is_empty()
    );
    // Mutations remain CSRF protected. They preserve only structured, locally rendered filters.
    let csrf = page_csrf(&all);
    for action in [
        "/incoming-changes/scan",
        "/incoming-changes/pr_other/pause",
        "/incoming-changes/pr_other/resume",
    ] {
        assert_eq!(
            browser_request(
                &w.base_url,
                action,
                Some(&cookie),
                Some(&w.base_url),
                Some(&[("csrf", "wrong")])
            )
            .0,
            403
        );
    }
    let fields = [
        ("csrf", csrf.as_str()),
        ("project", "application"),
        ("filter", "paused"),
        ("q", "dependency & more"),
        ("page", "2"),
    ];
    let (code, headers, body) = browser_request(
        &w.base_url,
        "/incoming-changes/pr_other/pause",
        Some(&cookie),
        Some(&w.base_url),
        Some(&fields),
    );
    assert_eq!(code, 303, "{body}");
    assert_eq!(
        headers["location"],
        "/incoming-changes?project=application&filter=paused&q=dependency%20%26%20more&page=2"
    );
    assert!(
        w.state
            .orchestrator
            .lock()
            .unwrap()
            .db
            .get_pull_request_source("pr_other")
            .unwrap()
            .paused
    );
    {
        let mut o = w.state.orchestrator.lock().unwrap();
        o.config
            .projects
            .get_mut("roles")
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .repositories
            .clear();
    }
    assert_eq!(
        browser_request(
            &w.base_url,
            "/incoming-changes/scan",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", csrf.as_str()), ("project", "roles")])
        )
        .0,
        303
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(format!("{}/incoming-changes/scan", w.base_url))
        .header("Cookie", &cookie)
        .header("Origin", &w.base_url)
        .header("Accept", "application/json")
        .send_form([("csrf", csrf.as_str()), ("project", "roles")])
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let report: serde_json::Value = response.body_mut().read_json().unwrap();
    assert_eq!(report["queued"], serde_json::json!([]));
    assert_eq!(report["errors"], serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn github_projects_allow_manual_tasks_without_manual_publication_effects() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        let f = o.config.forges.get_mut("fake").unwrap();
        f.provider = artidezi_config::model::ForgeProvider::Github;
        f.api_base_url = "https://api.github.com".into();
        f.github_app = Some(artidezi_config::model::GithubApp {
            client_id: "Iv.fixture".into(),
            installation_id: 42,
            account: "org".into(),
        });
        o.config
            .projects
            .get_mut("roles")
            .unwrap()
            .effects
            .publish_adaptation_merge_requests = false;
        assert!(artidezi_config::validate(&o.config).is_empty());
    })
    .await;
    let cookie = login_browser(&w.base_url);
    let (_, _, projects) = browser_request(&w.base_url, "/projects", Some(&cookie), None, None);
    assert!(
        !projects.contains("Dry run"),
        "No discovery rule is configured yet"
    );
    assert!(projects.contains("/tasks/new?project=roles"));
    for path in ["/tasks/new?project=roles", "/tasks/new"] {
        let (_, _, page) = browser_request(&w.base_url, path, Some(&cookie), None, None);
        assert!(!page.contains("Incoming changes only"));
        assert!(page.contains("id=\"create-task\""));
        assert!(page.contains(r#"<option value="roles" selected>roles</option>"#));
    }
    assert!(
        w.state
            .orchestrator
            .lock()
            .unwrap()
            .db
            .list_tasks(None)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn publication_retry_requires_a_session_and_csrf_before_accessing_a_task() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let cookie = login_browser(&w.base_url);
    for (cookie, origin) in [
        (None, Some(w.base_url.as_str())),
        (Some(cookie.as_str()), None),
        (Some(cookie.as_str()), Some(w.base_url.as_str())),
    ] {
        let (code, _, _) = browser_request(
            &w.base_url,
            "/tasks/unknown/retry-publication",
            cookie,
            origin,
            Some(&[("csrf", "invalid")]),
        );
        assert!(
            code == 303 || code == 403,
            "unauthorized request must not reach task lookup: {code}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn artifact_retention_admin_preview_apply_and_expired_attempt_ui() {
    let w = world_with_setup(Some(UiConfig::new(false)), |o| {
        o.config.instance.artifact_retention = Some(artidezi_config::model::ArtifactRetention {
            completed_task_bundle_retention_seconds: 86400,
            orphan_grace_seconds: 86400,
            cleanup_interval_seconds: 3600,
        });
        let task = o
            .create_manual_task("roles", "Retention fixture", "test", &Default::default())
            .unwrap();
        o.close_task(&task.id, "test").unwrap();
        // Simulate a pass interrupted after recording expiry but before unlinking inputs.
        o.db.expire_task_inputs(&task.id, chrono::Utc::now())
            .unwrap();
    })
    .await;
    let path = "/admin/v1/artifacts/cleanup";
    let (status, _) = admin(
        &w.base_url,
        "POST",
        path,
        Some(serde_json::json!({"apply": true})),
        "worker-secret",
    );
    assert_eq!(status, 403);
    let (status, preview) = admin(
        &w.base_url,
        "POST",
        path,
        Some(serde_json::json!({})),
        "admin-secret",
    );
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["dry_run"], true);
    assert!(preview["reclaimable_bytes"].as_u64().unwrap() > 0);
    let cookie = login_browser(&w.base_url);
    let (_, _, before) = browser_request(&w.base_url, "/tasks/1", Some(&cookie), None, None);
    assert!(!before.contains("Frozen repository inputs expired"));
    let (status, applied) = admin(
        &w.base_url,
        "POST",
        path,
        Some(serde_json::json!({"apply": true})),
        "admin-secret",
    );
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["removed_bytes"], preview["reclaimable_bytes"]);
    let (status, _, after) = browser_request(&w.base_url, "/tasks/1", Some(&cookie), None, None);
    assert_eq!(status, 200);
    assert!(after.contains("Frozen repository inputs expired"));
    let (status, repeated) = admin(
        &w.base_url,
        "POST",
        path,
        Some(serde_json::json!({"apply": true})),
        "admin-secret",
    );
    assert_eq!(status, 200, "{repeated}");
    assert_eq!(repeated["removed_bytes"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_configuration_decides_which_workers_exist() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let status = |id: &str, token: &str| {
        let body = serde_json::json!({"worker_pool":"personal","worker_id":id,"quarantined":false});
        admin(&w.base_url, "POST", "/worker/v1/status", Some(body), token)
    };
    assert_eq!(status("w1", "worker-secret").0, 204);
    let (code, body) = status("w2", "worker-secret");
    assert_eq!(code, 403);
    assert!(body.to_string().contains("personal/w1"));
    assert_eq!(status("unlisted", "worker-secret").0, 403);
    assert_eq!(status("w2", "not-a-token").0, 403);

    let cookie = login_browser(&w.base_url);
    let (_, _, page) = browser_request(&w.base_url, "/workers", Some(&cookie), None, None);
    assert_eq!(page.matches("Never connected").count(), 1);

    let mut config = w.state.orchestrator.lock().unwrap().config.clone();
    let pool = config.worker_pools.get_mut("personal").unwrap();
    let removed = pool.workers.remove("w1").unwrap();
    pool.workers.insert("w3".into(), removed);
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    forges.insert("fake".into(), Box::new(w.forge.clone()));
    let reopened = Orchestrator::open(config, &w.root.join("data"), forges).unwrap();
    assert!(reopened.db.list_workers().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_status_and_drain_are_authenticated_durable_and_csrf_protected() {
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    let status = serde_json::json!({"worker_pool":"personal","worker_id":"w1","quarantined":true});
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/status",
            Some(status.clone()),
            "admin-secret"
        )
        .0,
        403
    );
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/status",
            Some(status),
            "worker-secret"
        )
        .0,
        204
    );
    assert_eq!(
        admin(
            &w.base_url,
            "GET",
            "/admin/v1/workers",
            None,
            "worker-secret"
        )
        .0,
        403
    );
    let workers = admin(
        &w.base_url,
        "GET",
        "/admin/v1/workers",
        None,
        "admin-secret",
    )
    .1;
    assert_eq!(workers[0]["quarantined"], true);
    let cookie = login_browser(&w.base_url);
    let (code, _, page) = browser_request(&w.base_url, "/workers", Some(&cookie), None, None);
    assert_eq!(code, 200);
    assert!(page.contains("Quarantined"));
    assert!(page.contains("w1"));
    let csrf = page_csrf(&page);
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/admin/v1/maintenance",
            Some(serde_json::json!({"discovery_paused":true})),
            "worker-secret"
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            &w.base_url,
            "/workers/maintenance",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", "wrong"), ("discovery_paused", "true")])
        )
        .0,
        403
    );
    assert_eq!(
        browser_request(
            &w.base_url,
            "/workers/maintenance",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", csrf.as_str()), ("discovery_paused", "true")])
        )
        .0,
        303
    );
    assert_eq!(
        admin(
            &w.base_url,
            "GET",
            "/admin/v1/maintenance",
            None,
            "admin-secret"
        )
        .1["discovery_paused"],
        true
    );
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/admin/v1/maintenance",
            Some(serde_json::json!({"discovery_paused":false})),
            "admin-secret"
        )
        .0,
        204
    );
    let mut form = vec![
        ("csrf", "wrong"),
        ("worker_pool", "personal"),
        ("worker_id", "w1"),
        ("drained", "true"),
    ];
    assert_eq!(
        browser_request(
            &w.base_url,
            "/workers/drain",
            Some(&cookie),
            Some(&w.base_url),
            Some(&form)
        )
        .0,
        403
    );
    form[0] = ("csrf", &csrf);
    assert_eq!(
        browser_request(
            &w.base_url,
            "/workers/drain",
            Some(&cookie),
            Some(&w.base_url),
            Some(&form)
        )
        .0,
        303
    );
    let workers = admin(
        &w.base_url,
        "GET",
        "/admin/v1/workers",
        None,
        "admin-secret",
    )
    .1;
    assert_eq!(workers[0]["drained"], true);
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/status",
            Some(
                serde_json::json!({"worker_pool":"personal","worker_id":"w1","quarantined":false})
            ),
            "worker-secret"
        )
        .0,
        204
    );
    let workers = admin(
        &w.base_url,
        "GET",
        "/admin/v1/workers",
        None,
        "admin-secret",
    )
    .1;
    assert_eq!(workers[0]["drained"], true);
    assert_eq!(workers[0]["quarantined"], false);
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/admin/v1/workers/drain",
            Some(serde_json::json!({"worker_pool":"personal","worker_id":"w1","drained":false})),
            "admin-secret"
        )
        .0,
        204
    );
}
