// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The standalone worker loop: lease, fetch inputs, run the attempt, upload, report, clean up.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use artidezi_contract::common::{ArtifactRef, RepositoryId};
use artidezi_contract::job::Harness;
use artidezi_contract::wire::*;
use artidezi_harness::HarnessAdapter;
use artidezi_harness::codex::CodexHarness;
use artidezi_harness::fake::FakeHarness;
use artidezi_harness::pi::PiHarness;

use crate::client::Client;
use crate::{
    AttemptInputs, Cancellation, PreviousInputs, Result, WorkerConfig, WorkerError, io_err,
    run_attempt_cancellable,
};

const RETRY_PAUSE: Duration = Duration::from_secs(1);
const HEARTBEAT_RETRY_PAUSE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub orchestrator_url: String,
    /// Permits sending the token over plain HTTP to a non-loopback host.
    pub allow_insecure_orchestrator_url: bool,
    pub token: String,
    pub worker: WorkerConfig,
    pub poll_interval: Duration,
    /// Tags advertised to the orchestrator, which projects can require.
    pub tags: Vec<String>,
    /// Path of the Codex binary, when this worker offers the `codex` harness.
    pub codex_binary: Option<PathBuf>,
    pub pi_binary: Option<PathBuf>,
    /// Path of a scripted fake harness, for test deployments only.
    pub fake_script: Option<PathBuf>,
}

pub struct Daemon {
    config: DaemonConfig,
    client: Client,
    harnesses: BTreeMap<Harness, Box<dyn HarnessAdapter>>,
    quarantined: AtomicBool,
    shutdown: Cancellation,
}

struct Heartbeat {
    cancellation: Cancellation,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn is_secure_url(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.split(':').next().unwrap_or(authority),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Result<Self> {
        if !config.allow_insecure_orchestrator_url && !is_secure_url(&config.orchestrator_url) {
            return Err(WorkerError::Invalid(
                "orchestrator_url must use https unless it is a loopback address".into(),
            ));
        }
        let client = Client::new(&config.orchestrator_url, &config.token);
        let mut harnesses: BTreeMap<Harness, Box<dyn HarnessAdapter>> = BTreeMap::new();
        if let Some(binary) = &config.codex_binary {
            harnesses.insert(
                Harness::Codex,
                Box::new(CodexHarness {
                    binary: binary.clone(),
                }),
            );
        }
        if let Some(binary) = &config.pi_binary {
            harnesses.insert(
                Harness::Pi,
                Box::new(PiHarness {
                    binary: binary.clone(),
                }),
            );
        }
        if let Some(script) = &config.fake_script {
            harnesses.insert(
                Harness::Fake,
                Box::new(FakeHarness {
                    script: script.clone(),
                }),
            );
        }
        if harnesses.is_empty() {
            return Err(WorkerError::Invalid("no harness configured".into()));
        }
        Ok(Self {
            config,
            client,
            harnesses,
            quarantined: AtomicBool::new(false),
            shutdown: Cancellation::default(),
        })
    }

    fn quarantine_path(&self) -> PathBuf {
        self.config
            .worker
            .workspaces_root
            .join("worker-quarantine.json")
    }

    fn ensure_healthy(&self) -> Result<()> {
        let path = self.quarantine_path();
        if self.quarantined.load(Ordering::Relaxed) || path.try_exists().map_err(io_err(&path))? {
            self.report_status(true);
            return Err(WorkerError::Quarantined { path });
        }
        Ok(())
    }

    fn quarantine(&self, attempt_id: &str, error: &WorkerError) {
        // Latch before filesystem I/O; failure to persist must not permit another lease.
        self.quarantined.store(true, Ordering::Relaxed);
        let path = self.quarantine_path();
        let record = serde_json::json!({
            "at": chrono::Utc::now(),
            "attempt_id": attempt_id,
            "worker_id": self.config.worker.worker_id,
            "reason": error.to_string(),
        });
        let persist = || -> Result<()> {
            std::fs::create_dir_all(&self.config.worker.workspaces_root).map_err(io_err(&path))?;
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&record).expect("JSON value"),
            )
            .map_err(io_err(&path))
        };
        if let Err(error) = persist() {
            tracing::error!(%error, "could not persist worker quarantine marker");
        }
        self.report_status(true);
        tracing::error!(attempt = attempt_id, %error, marker = %path.display(),
            "worker quarantined; heartbeats stopped; recover environment before removing marker and restarting worker");
    }

    fn advertised(&self) -> Result<Vec<AdvertisedHarness>> {
        let mut list = vec![];
        for (harness, adapter) in &self.harnesses {
            list.push(AdvertisedHarness {
                name: format!("{harness:?}").to_lowercase(),
                version: adapter.installed_version().map_err(WorkerError::Harness)?,
            });
        }
        Ok(list)
    }

    fn report_status(&self, quarantined: bool) {
        if let Err(error) = self.client.status(&WorkerStatusRequest {
            worker_pool: self.config.worker.worker_pool.clone(),
            worker_id: self.config.worker.worker_id.clone(),
            quarantined,
        }) {
            tracing::warn!(%error, "worker presence report failed");
        }
    }

    /// Leases and processes at most one attempt. Returns whether there was work.
    pub fn run_once(&self) -> Result<bool> {
        self.ensure_healthy()?;
        self.report_status(false);
        let advertised = self.advertised().inspect_err(|error| {
            if matches!(
                error,
                WorkerError::Harness(artidezi_harness::HarnessError::UnstoppableProcess { .. })
            ) {
                self.quarantine("preflight", error);
            }
        })?;
        let request = LeaseRequest {
            worker_pool: self.config.worker.worker_pool.clone(),
            worker_id: self.config.worker.worker_id.clone(),
            supported_job_schema_versions: artidezi_contract::job::SUPPORTED_JOB_SCHEMA_VERSIONS
                .to_vec(),
            harnesses: advertised,
            tags: self.config.tags.clone(),
        };
        let Some(lease) = self.client.lease(&request)? else {
            return Ok(false);
        };
        let attempt_id = lease.attempt_id.to_string();
        if !artidezi_contract::validate::is_plain_id(&attempt_id) {
            return Err(WorkerError::Invalid(format!(
                "attempt id {attempt_id:?} is not a plain name"
            )));
        }
        tracing::info!(attempt = %attempt_id, number = lease.attempt_number, "leased attempt");

        let inputs_dir = self
            .config
            .worker
            .workspaces_root
            .join("inputs")
            .join(&attempt_id);
        let heartbeat = self.heartbeat(&lease);
        let result = self.process(&lease, &inputs_dir, &heartbeat.cancellation);
        drop(heartbeat);
        if let Err(
            error @ (WorkerError::UnstoppableProcess { .. }
            | WorkerError::Harness(artidezi_harness::HarnessError::UnstoppableProcess {
                ..
            })),
        ) = &result
        {
            self.quarantine(&attempt_id, error);
        } else {
            let _ = std::fs::remove_dir_all(&inputs_dir);
            // Retained workspaces help debugging, but must not keep credential copies.
            let home = self
                .config
                .worker
                .workspaces_root
                .join(&attempt_id)
                .join("home");
            let _ = std::fs::remove_dir_all(home);
        }
        match result {
            Err(WorkerError::Cancelled) => {
                tracing::info!(attempt = %attempt_id, "attempt revoked; stopped work and retained workspace");
                Ok(true)
            }
            Ok(state) => {
                tracing::info!(attempt = %attempt_id, state, "attempt reported");
                Ok(true)
            }
            Err(e) => {
                tracing::error!(attempt = %attempt_id, "attempt processing failed: {e}");
                Err(e)
            }
        }
    }

    fn heartbeat(&self, lease: &LeaseResponse) -> Heartbeat {
        let cancellation = Cancellation::default();
        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat = {
            let stop = stop.clone();
            let cancellation = cancellation.clone();
            let shutdown = self.shutdown.clone();
            let client = Client::new(&self.config.orchestrator_url, &self.config.token);
            let attempt_id = lease.attempt_id.to_string();
            let worker_id = self.config.worker.worker_id.clone();
            let lease_ttl_seconds = lease.lease_ttl_seconds;
            let every = Duration::from_secs((lease_ttl_seconds / 3).clamp(1, 30));
            // Renew as soon as the orchestrator returns: the lease may be about to expire.
            let retry = every.min(HEARTBEAT_RETRY_PAUSE);
            std::thread::spawn(move || {
                let mut last_confirmed = Instant::now();
                let ttl = Duration::from_secs(lease_ttl_seconds);
                let step = Duration::from_millis(250);
                let mut wait = every;
                'outer: loop {
                    // Sleep in small steps so the join after the attempt returns promptly.
                    let mut slept = Duration::ZERO;
                    while slept < wait {
                        if stop.load(Ordering::Relaxed) {
                            break 'outer;
                        }
                        if shutdown.is_cancelled() {
                            cancellation.cancel();
                            break 'outer;
                        }
                        std::thread::sleep(step);
                        slept += step;
                    }
                    match client.heartbeat(&attempt_id, &worker_id) {
                        Err(WorkerError::Cancelled) => {
                            cancellation.cancel();
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(attempt = %attempt_id, "heartbeat failed: {e}");
                            if last_confirmed.elapsed() >= ttl {
                                cancellation.cancel();
                                break;
                            }
                            wait = retry;
                        }
                        Ok(()) => {
                            last_confirmed = Instant::now();
                            wait = every;
                        }
                    }
                }
            })
        };

        Heartbeat {
            cancellation,
            stop,
            thread: Some(heartbeat),
        }
    }

    fn process(
        &self,
        lease: &LeaseResponse,
        inputs_dir: &std::path::Path,
        cancellation: &Cancellation,
    ) -> Result<String> {
        let attempt_id = lease.attempt_id.to_string();
        let job = &lease.job;
        let problems = artidezi_contract::validate::validate_job(job);
        if !problems.is_empty() {
            return Err(WorkerError::Invalid(format!(
                "orchestrator sent an invalid job: {problems:?}"
            )));
        }
        let adapter = self
            .harnesses
            .get(&job.agent_profile.harness)
            .ok_or_else(|| {
                WorkerError::Invalid(format!(
                    "harness {:?} is not configured",
                    job.agent_profile.harness
                ))
            })?;

        let lease_ttl = Duration::from_secs(lease.lease_ttl_seconds);
        let download = |artifact: &ArtifactRef, path: &std::path::Path| {
            self.retrying(lease_ttl, cancellation, || {
                self.client.download(&attempt_id, artifact, path)
            })
        };

        // Fetch every bundle the job references.
        std::fs::create_dir_all(inputs_dir).map_err(io_err(inputs_dir))?;
        let mut snapshot_bundles = BTreeMap::new();
        let mut input_ref_bundles = BTreeMap::new();
        for repo in &job.repositories {
            let path = inputs_dir.join(format!("{}.snapshot.bundle", repo.id));
            download(&repo.snapshot_bundle, &path)?;
            snapshot_bundles.insert(repo.id.clone(), path);
            for input in &repo.input_refs {
                let path = inputs_dir.join(format!("{}.{}.bundle", repo.id, input.name));
                download(&input.bundle, &path)?;
                input_ref_bundles.insert((repo.id.clone(), input.name.clone()), path);
            }
        }
        let previous = match &job.previous_attempt {
            Some(prev) => {
                let doc_path = inputs_dir.join("previous-attempt.json");
                download(&prev.attempt_document, &doc_path)?;
                let result_document = match &prev.result_document {
                    Some(r) => {
                        let p = inputs_dir.join("previous-result.json");
                        download(r, &p)?;
                        Some(std::fs::read(&p).map_err(io_err(&p))?)
                    }
                    None => None,
                };
                let mut commit_bundles: BTreeMap<RepositoryId, PathBuf> = BTreeMap::new();
                for (repo_id, bundle) in &prev.commit_bundles {
                    let p = inputs_dir.join(format!("previous-{repo_id}.bundle"));
                    download(bundle, &p)?;
                    commit_bundles.insert(repo_id.clone(), p);
                }
                Some(PreviousInputs {
                    attempt_document: std::fs::read(&doc_path).map_err(io_err(&doc_path))?,
                    result_document,
                    commit_bundles,
                })
            }
            None => None,
        };
        let job_bytes =
            serde_json::to_vec_pretty(job).map_err(|e| WorkerError::Invalid(e.to_string()))?;
        // The record must reference the job by the orchestrator's stored hash, which may differ
        // from our re-serialization; run_attempt hashes what it is given.
        let job_bytes = match self.fetch_job_bytes(&attempt_id, lease, cancellation) {
            Ok(bytes) => bytes,
            Err(_) => job_bytes,
        };

        cancellation.check()?;
        // Recheck after downloads before starting any repository commands. A transient failure
        // is advisory; the controller remains authoritative when uploads are attempted.
        self.check_lease(&attempt_id)?;

        let outcome = run_attempt_cancellable(
            &self.config.worker,
            adapter.as_ref(),
            &AttemptInputs {
                job,
                job_bytes: &job_bytes,
                task_document: lease.task_document.as_bytes(),
                snapshot_bundles,
                input_ref_bundles,
                previous,
            },
            cancellation,
        );
        let output = outcome?;
        cancellation.check()?;
        self.check_lease(&attempt_id)?;

        // Upload, then report; the workspace is destroyed only after the orchestrator accepted.
        let mut artifacts = BTreeMap::new();
        for staged in &output.artifacts {
            cancellation.check()?;
            let stored = self.retrying(lease_ttl, cancellation, || {
                self.client
                    .upload(&attempt_id, &staged.artifact, &staged.path)
            })?;
            artifacts.insert(staged.name.clone(), stored);
        }
        let record_path = output.workspace.join("out/attempt.json");
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_path).map_err(io_err(&record_path))?)
                .map_err(|e| WorkerError::Invalid(e.to_string()))?;
        let request = RecordRequest {
            worker_id: self.config.worker.worker_id.clone(),
            record,
            artifacts,
        };
        let response = self.retrying(lease_ttl, cancellation, || {
            self.client.record(&attempt_id, &request)
        })?;
        output.cleanup()?;
        Ok(response.attempt_state)
    }

    /// Waits out an orchestrator restart instead of giving up on the attempt.
    /// The lease is what the orchestrator would wait for, so waiting longer gains nothing.
    fn retrying<T>(
        &self,
        patience: Duration,
        cancellation: &Cancellation,
        mut send: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + patience;
        loop {
            cancellation.check()?;
            match send() {
                Err(error) if error.is_transient() && Instant::now() < deadline => {
                    tracing::warn!("retrying: {error}");
                    self.pause(RETRY_PAUSE);
                }
                result => return result,
            }
        }
    }

    fn check_lease(&self, attempt_id: &str) -> Result<()> {
        match self
            .client
            .heartbeat(attempt_id, &self.config.worker.worker_id)
        {
            Err(WorkerError::Cancelled) => Err(WorkerError::Cancelled),
            Err(error) => {
                tracing::warn!(attempt = %attempt_id, "heartbeat failed: {error}");
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }

    /// The stored `job.json` bytes, so `attempt.json` can reference the same hash.
    fn fetch_job_bytes(
        &self,
        attempt_id: &str,
        lease: &LeaseResponse,
        cancellation: &Cancellation,
    ) -> Result<Vec<u8>> {
        let path = self
            .config
            .worker
            .workspaces_root
            .join("inputs")
            .join(attempt_id)
            .join("job.json");
        let lease_ttl = Duration::from_secs(lease.lease_ttl_seconds);
        self.retrying(lease_ttl, cancellation, || {
            self.client.download(attempt_id, &lease.job_artifact, &path)
        })?;
        std::fs::read(&path).map_err(io_err(&path))
    }

    /// Polls until SIGTERM, which also stops any running attempt. Errors are logged and retried.
    pub fn run(&self) -> Result<()> {
        self.shutdown
            .on_terminate()
            .map_err(|e| WorkerError::Invalid(format!("handling SIGTERM: {e}")))?;
        while !self.shutdown.is_cancelled() {
            match self.run_once() {
                Ok(true) => continue,
                Ok(false) => self.pause(self.config.poll_interval),
                Err(WorkerError::Quarantined { path }) => {
                    tracing::error!(marker = %path.display(), "worker quarantined; recovery and supervisor restart required");
                    while !self.shutdown.is_cancelled() {
                        self.report_status(true);
                        self.pause(Duration::from_secs(30));
                    }
                }
                Err(e) => {
                    tracing::error!("worker iteration failed: {e}");
                    self.pause(self.config.poll_interval);
                }
            }
        }
        tracing::info!("worker stopped");
        Ok(())
    }

    fn pause(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.shutdown.is_cancelled() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_http_is_only_accepted_on_loopback() {
        for url in [
            "https://orchestrator.example",
            "http://127.0.0.1:8080",
            "http://localhost:8080/",
            "http://[::1]:8080",
        ] {
            assert!(super::is_secure_url(url), "{url}");
        }
        for url in [
            "http://orchestrator:8080",
            "http://localhost.example",
            "ftp://x",
        ] {
            assert!(!super::is_secure_url(url), "{url}");
        }
    }

    use super::*;

    fn daemon(root: &std::path::Path) -> Daemon {
        Daemon::new(DaemonConfig {
            orchestrator_url: "http://127.0.0.1:1".into(),
            allow_insecure_orchestrator_url: false,
            token: "test".into(),
            worker: WorkerConfig {
                worker_pool: "test".into(),
                worker_id: "test".into(),
                workspaces_root: root.to_owned(),
                credentials_root: None,
                max_log_bytes: 1024,
            },
            poll_interval: Duration::from_millis(1),
            tags: vec![],
            codex_binary: None,
            pi_binary: None,
            fake_script: Some("/bin/true".into()),
        })
        .unwrap()
    }

    #[test]
    fn retrying_waits_out_an_orchestrator_restart() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(dir.path());
        let mut failures = vec![
            WorkerError::Unreachable("connection refused".into()),
            WorkerError::Http {
                operation: "record".into(),
                status: 502,
                message: "bad gateway".into(),
            },
            WorkerError::Http {
                operation: "record".into(),
                status: 404,
                message: "404 page not found".into(),
            },
        ];
        let result = daemon.retrying(Duration::from_secs(60), &Cancellation::default(), || {
            failures.pop().map_or(Ok("accepted"), Err)
        });
        assert_eq!(result.unwrap(), "accepted");
    }

    #[test]
    fn retrying_stops_at_a_refusal_or_the_deadline_or_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(dir.path());
        let mut tries = 0;
        let refused = daemon.retrying(Duration::from_secs(60), &Cancellation::default(), || {
            tries += 1;
            Err::<(), _>(WorkerError::Http {
                operation: "record".into(),
                status: 409,
                message: "attempt is lost".into(),
            })
        });
        assert!(matches!(
            refused,
            Err(WorkerError::Http { status: 409, .. })
        ));
        assert_eq!(tries, 1);

        let expired = daemon.retrying(Duration::ZERO, &Cancellation::default(), || {
            Err::<(), _>(WorkerError::Unreachable("connection refused".into()))
        });
        assert!(matches!(expired, Err(WorkerError::Unreachable(_))));

        let cancellation = Cancellation::default();
        cancellation.cancel();
        let revoked = daemon.retrying(Duration::from_secs(60), &cancellation, || Ok(()));
        assert!(matches!(revoked, Err(WorkerError::Cancelled)));
    }

    #[test]
    fn quarantine_blocks_new_leases_and_survives_supervisor_restart() {
        let dir = tempfile::tempdir().unwrap();
        let worker = daemon(dir.path());
        worker.quarantine(
            "att_stuck",
            &WorkerError::UnstoppableProcess {
                program: "/bin/sleep".into(),
                pid: 123,
            },
        );
        assert!(matches!(
            worker.run_once(),
            Err(WorkerError::Quarantined { .. })
        ));
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(worker.quarantine_path()).unwrap()).unwrap();
        assert_eq!(record["attempt_id"], "att_stuck");
        assert!(matches!(
            daemon(dir.path()).run_once(),
            Err(WorkerError::Quarantined { .. })
        ));
        std::fs::remove_file(worker.quarantine_path()).unwrap();
        // Clearing the file cannot restart the still-running quarantined supervisor.
        assert!(matches!(
            worker.run_once(),
            Err(WorkerError::Quarantined { .. })
        ));
        assert!(daemon(dir.path()).ensure_healthy().is_ok());
    }
}
