// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use super::*;
use artidezi_contract::common::CommitSha;
use artidezi_forge::*;
use artidezi_git::cache::HttpsCredentials;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

/// Hold one real repository preparation immediately before its Git fetch. No timing sleeps:
/// the test waits for entry and explicitly releases it. Drop and a timeout prevent stuck tests.
#[derive(Default)]
pub(super) struct Gate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

impl Gate {
    fn arm(self: &Arc<Self>) -> Release {
        *self.released.lock().unwrap() = false;
        self.armed.store(true, Ordering::SeqCst);
        Release(self.clone())
    }

    fn block_once(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let (released, timeout) = self
                .wake
                .wait_timeout_while(
                    self.released.lock().unwrap(),
                    Duration::from_secs(20),
                    |released| !*released,
                )
                .unwrap();
            assert!(
                *released && !timeout.timed_out(),
                "test did not release repository preparation"
            );
        }
    }

    async fn wait_until_blocked(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }
}

struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

pub(super) struct GatedForge {
    pub inner: FakeForge,
    pub gate: Arc<Gate>,
}

impl ForgeAdapter for GatedForge {
    fn protected_paths(&self) -> &'static [&'static str] {
        self.inner.protected_paths()
    }
    fn proposals_for_branch(
        &self,
        remote: &str,
        branch: &str,
    ) -> artidezi_forge::Result<Vec<artidezi_forge::MergeRequestInfo>> {
        self.inner.proposals_for_branch(remote, branch)
    }
    fn writes_allowed(&self) -> bool {
        self.inner.writes_allowed()
    }

    fn clone_url(&self, remote: &str) -> String {
        self.gate.block_once();
        self.inner.clone_url(remote)
    }
    fn git_credentials(&self, remote: &str) -> Result<Option<HttpsCredentials>> {
        self.inner.git_credentials(remote)
    }

    fn branch_head(&self, remote: &str, branch: &str) -> Result<CommitSha> {
        self.inner.branch_head(remote, branch)
    }

    fn merge_request_ref(&self, remote: &str, number: u64) -> Result<String> {
        self.inner.merge_request_ref(remote, number)
    }

    fn create_merge_request(
        &self,
        remote: &str,
        request: &NewMergeRequest,
    ) -> Result<MergeRequestInfo> {
        self.inner.create_merge_request(remote, request)
    }

    fn update_merge_request(
        &self,
        remote: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<()> {
        self.inner.update_merge_request(remote, number, title, body)
    }

    fn get_merge_request(&self, remote: &str, number: u64) -> Result<MergeRequestInfo> {
        self.inner.get_merge_request(remote, number)
    }

    fn list_open_merge_requests(
        &self,
        remote: &str,
        author: Option<&str>,
    ) -> Result<Vec<MergeRequestInfo>> {
        self.inner.list_open_merge_requests(remote, author)
    }

    fn merge(
        &self,
        remote: &str,
        number: u64,
        expected_head: &CommitSha,
        method: MergeMethod,
    ) -> Result<CommitSha> {
        self.inner.merge(remote, number, expected_head, method)
    }

    fn create_tag(&self, remote: &str, name: &str, commit: &CommitSha) -> Result<()> {
        self.inner.create_tag(remote, name, commit)
    }
}

fn lease_body(worker: &str) -> serde_json::Value {
    serde_json::json!({
        "worker_id": worker, "worker_pool": "personal",
        "supported_job_schema_versions": [1, 2],
        "harnesses": [{"name": "fake", "version": "fake-1"}]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_preparation_keeps_ui_leases_heartbeats_and_artifacts_responsive() {
    use sha2::{Digest, Sha256};
    let w = world_with_ui(Some(UiConfig::new(false))).await;
    for _ in 0..2 {
        assert_eq!(
            admin(
                &w.base_url,
                "POST",
                "/admin/v1/tasks",
                Some(serde_json::json!({"project": "roles", "instructions": "existing work"})),
                "admin-secret"
            )
            .0,
            200
        );
    }
    let (code, first) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease_body("w1")),
        "worker-secret",
    );
    assert_eq!(code, 200);
    let attempt = first["attempt_id"].as_str().unwrap();
    let hold = w.gate.arm();
    let base = w.base_url.clone();
    let pending = tokio::task::spawn_blocking(move || {
        admin(
            &base,
            "POST",
            "/admin/v1/tasks",
            Some(serde_json::json!({"project": "roles", "instructions": "slow import"})),
            "admin-secret",
        )
    });
    w.gate.wait_until_blocked().await;
    assert!(w.state.orchestrator.try_lock().is_err());

    // Real HTTP responses must arrive while the import remains blocked, not after its release.
    let started = std::time::Instant::now();
    for path in ["/admin/v1/tasks", "/admin/v1/discovery"] {
        assert_eq!(admin(&w.base_url, "GET", path, None, "admin-secret").0, 200);
        assert_eq!(
            admin(&w.base_url, "GET", path, None, "worker-secret").0,
            403
        );
    }
    let cookie = login_browser(&w.base_url);
    for path in [
        "/tasks",
        "/tasks/1",
        "/projects",
        "/agent-profiles",
        "/incoming-changes",
    ] {
        assert_eq!(
            browser_request(&w.base_url, path, Some(&cookie), None, None).0,
            200,
            "{path}"
        );
    }
    assert_eq!(
        browser_request(&w.base_url, "/tasks", None, None, None).0,
        303
    );
    let (code, second) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease_body("w2")),
        "worker-2-secret",
    );
    assert_eq!(code, 200);
    assert_ne!(first["attempt_id"], second["attempt_id"]);
    for worker in ["w1", "w2"] {
        let id = if worker == "w1" {
            attempt
        } else {
            second["attempt_id"].as_str().unwrap()
        };
        assert_eq!(
            admin(
                &w.base_url,
                "POST",
                &format!("/worker/v1/attempts/{id}/heartbeat"),
                Some(serde_json::json!({"worker_id": worker})),
                if worker == "w1" {
                    "worker-secret"
                } else {
                    "worker-2-secret"
                }
            )
            .0,
            204
        );
    }
    let snapshot = first["job"]["repositories"][0]["snapshot_bundle"]["sha256"]
        .as_str()
        .unwrap();
    assert_eq!(
        admin(
            &w.base_url,
            "GET",
            &format!("/worker/v1/attempts/{attempt}/artifacts/{snapshot}"),
            None,
            "worker-secret"
        )
        .0,
        200
    );
    assert_eq!(
        admin(
            &w.base_url,
            "GET",
            &format!("/worker/v1/attempts/{attempt}/artifacts/{snapshot}"),
            None,
            "worker-2-secret"
        )
        .0,
        403
    );
    assert_eq!(
        admin(
            &w.base_url,
            "GET",
            &format!("/worker/v1/attempts/{attempt}/artifacts/{}", "a".repeat(64)),
            None,
            "worker-secret"
        )
        .0,
        404
    );
    let payload = b"worker output during import";
    let hash = hex::encode(Sha256::digest(payload));
    let client: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into();
    assert_eq!(
        client
            .put(format!(
                "{}/worker/v1/attempts/{attempt}/artifacts/{hash}",
                w.base_url
            ))
            .header("Authorization", "Bearer worker-secret")
            .send(payload.as_slice())
            .unwrap()
            .status(),
        200
    );
    let (_, _, task_page) = browser_request(&w.base_url, "/tasks/1", Some(&cookie), None, None);
    let csrf = page_csrf(&task_page);
    assert_eq!(
        browser_request(
            &w.base_url,
            "/logout",
            Some(&cookie),
            Some(&w.base_url),
            Some(&[("csrf", &csrf)])
        )
        .0,
        303
    );
    assert_eq!(
        browser_request(&w.base_url, "/tasks", Some(&cookie), None, None).0,
        303
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(w.state.orchestrator.try_lock().is_err());
    drop(hold);
    let (code, created) = pending.await.unwrap();
    assert_eq!(code, 200, "{created}");
    let o = w.state.orchestrator.lock().unwrap();
    assert_eq!(o.db.list_tasks(None).unwrap().len(), 3);
    assert_eq!(
        o.db.list_attempts(created["id"].as_str().unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_scans_keep_single_source_ownership_and_cancellation() {
    use artidezi_store::model::{AttemptState, TaskState};
    let w = world_with_setup(None, |o| {
        o.config.projects.get_mut("roles").unwrap().pull_requests = Some(
            serde_json::from_value(serde_json::json!({
                "poll_interval_seconds": 600, "settle_seconds": 0, "max_runs_per_pull_request": 3,
                "max_active_tasks": 1, "repositories": ["role"], "handlers": [{
                    "id": "renovate", "authors": ["renovate[bot]"], "action": "assess",
                    "wait_for_checks_to_finish": false,
                    "require_successful_checks": false, "instructions": "Review the update"
                }]
            }))
            .unwrap(),
        );
    })
    .await;
    let repo = w.forge.ensure_repository("org/role").unwrap();
    let base_commit = repo.refname_to_id("refs/heads/main").unwrap();
    let head = seed(&repo, Some(base_commit), "VERSION", "2\n", "Bump version");
    repo.reference("refs/heads/renovate/update", head, true, "source")
        .unwrap();
    repo.reference("refs/heads/main", base_commit, true, "target")
        .unwrap();
    w.forge
        .open_merge_request_as(
            "org/role",
            "renovate[bot]",
            &NewMergeRequest {
                title: "Update dependency".into(),
                body: "Version bump".into(),
                source_branch: "renovate/update".into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap();
    let hold = w.gate.arm();
    let base = w.base_url.clone();
    let first = tokio::task::spawn_blocking(move || {
        admin(
            &base,
            "POST",
            "/admin/v1/discovery/scan",
            Some(serde_json::json!({"project": "roles"})),
            "admin-secret",
        )
    });
    w.gate.wait_until_blocked().await;
    let base = w.base_url.clone();
    let second = tokio::task::spawn_blocking(move || {
        admin(
            &base,
            "POST",
            "/admin/v1/discovery/scan",
            Some(serde_json::json!({"project": "roles"})),
            "admin-secret",
        )
    });
    let (code, sources) = admin(
        &w.base_url,
        "GET",
        "/admin/v1/discovery",
        None,
        "admin-secret",
    );
    assert_eq!(code, 200);
    // The first source/task/attempt claim is committed together after preparation.
    assert!(sources.as_array().unwrap().is_empty());
    assert!(!first.is_finished());
    drop(hold);
    assert_eq!(first.await.unwrap().0, 200);
    assert_eq!(second.await.unwrap().0, 200);
    let (task, attempt) = {
        let o = w.state.orchestrator.lock().unwrap();
        let tasks = o.db.list_tasks(None).unwrap();
        assert_eq!(tasks.len(), 1);
        let task = tasks[0].clone();
        let attempts = o.db.list_attempts(&task.id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(o.db.list_pull_request_sources(None).unwrap()[0].runs, 1);
        (task, attempts[0].clone())
    };
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/lease",
            Some(lease_body("w1")),
            "worker-secret"
        )
        .0,
        200
    );
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            &format!("/admin/v1/tasks/{}/close", task.id),
            Some(serde_json::json!({"actor": "admin"})),
            "admin-secret"
        )
        .0,
        200
    );
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            &format!("/worker/v1/attempts/{}/heartbeat", attempt.id),
            Some(serde_json::json!({"worker_id": "w1"})),
            "worker-secret"
        )
        .0,
        409
    );
    let o = w.state.orchestrator.lock().unwrap();
    assert_eq!(o.db.get_task(&task.id).unwrap().state, TaskState::Closed);
    assert_eq!(
        o.db.get_attempt(&attempt.id).unwrap().state,
        AttemptState::Cancelled
    );
    assert!(o.db.get_lease(&attempt.id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_lease_recovery_and_replacement_dispatch_do_not_wait_for_slow_preparation() {
    let w = world_with_setup(None, |o| {
        o.config
            .worker_pools
            .get_mut("personal")
            .unwrap()
            .lost_attempt_retries = 2;
    })
    .await;
    let (_, task) = admin(
        &w.base_url,
        "POST",
        "/admin/v1/tasks",
        Some(serde_json::json!({"project":"roles","instructions":"recover me"})),
        "admin-secret",
    );
    let (code, first) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease_body("w1")),
        "worker-secret",
    );
    assert_eq!(code, 200);
    let hold = w.gate.arm();
    let base = w.base_url.clone();
    let pending = tokio::task::spawn_blocking(move || {
        admin(
            &base,
            "POST",
            "/admin/v1/tasks",
            Some(serde_json::json!({"project":"roles","instructions":"slow import"})),
            "admin-secret",
        )
    });
    w.gate.wait_until_blocked().await;
    let now = chrono::Utc::now() + chrono::Duration::seconds(61);
    let lost = tokio::time::timeout(
        Duration::from_secs(2),
        artidezi_server::with_state(&w.state, move |o| Ok(o.recover_expired_at(now)?)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lost, [first["attempt_id"].as_str().unwrap()]);
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            &format!("/worker/v1/attempts/{}/heartbeat", lost[0]),
            Some(serde_json::json!({"worker_id":"w1"})),
            "worker-secret"
        )
        .0,
        409
    );
    let (code, retry) = admin(
        &w.base_url,
        "POST",
        "/worker/v1/lease",
        Some(lease_body("w2")),
        "worker-2-secret",
    );
    assert_eq!(code, 200);
    assert_eq!(retry["job"]["task_id"], task["id"]);
    assert_ne!(retry["attempt_id"], first["attempt_id"]);
    assert_eq!(retry["attempt_number"], 2);
    // The failed worker cannot pick up another attempt during its cooldown.
    assert_eq!(
        admin(
            &w.base_url,
            "POST",
            "/worker/v1/lease",
            Some(lease_body("w1")),
            "worker-secret"
        )
        .0,
        204
    );
    drop(hold);
    assert_eq!(pending.await.unwrap().0, 200);
}
