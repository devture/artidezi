// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Worker-facing side of the orchestrator: leasing, heartbeats and ingestion of attempt records.
//! Everything a worker uploads is re-validated here from the raw artifacts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use artidezi_config::ResolvedProjectPolicy;
use artidezi_contract::Job;
use artidezi_contract::attempt::{AttemptRecord, AttemptStatus};
use artidezi_contract::common::{ArtifactRef, RepositoryAccess, RepositoryId, SCHEMA_VERSION};
use artidezi_contract::result::AgentResult;
use artidezi_contract::validate::validate_result;
use artidezi_git::bundle;
use artidezi_git::publish::{ContentRules, inspect};
use artidezi_store::model::*;
use chrono::Duration;
use git2::Oid;
use sha2::{Digest, Sha256};

use crate::state::StateAccess;
use crate::{Orchestrator, OrchestratorError, Result};

pub struct PreviousFiles {
    pub attempt_document: Vec<u8>,
    pub result_document: Option<Vec<u8>>,
    pub commit_bundles: BTreeMap<RepositoryId, PathBuf>,
}

/// A leased attempt with every input a worker needs, bundles as paths into the artifact store.
pub struct LeasedWork {
    pub attempt: Attempt,
    pub job: Job,
    pub job_bytes: Vec<u8>,
    pub task_document: Vec<u8>,
    pub snapshot_bundles: BTreeMap<RepositoryId, PathBuf>,
    pub input_ref_bundles: BTreeMap<(RepositoryId, String), PathBuf>,
    pub previous: Option<PreviousFiles>,
}

impl Orchestrator {
    pub fn lease(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
    ) -> Result<Option<LeasedWork>> {
        self.state_access().lease(worker_pool, worker_id, ttl)
    }

    pub fn heartbeat(&mut self, attempt_id: &str, worker_id: &str, ttl: Duration) -> Result<()> {
        self.state_access().heartbeat(attempt_id, worker_id, ttl)
    }

    /// Stores the uploaded artifacts and record, validates everything, judges the attempt and
    /// applies the outcome (publication or a request for human attention).
    pub fn ingest(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        record_bytes: &[u8],
        artifact_files: &[(String, PathBuf)],
    ) -> Result<AttemptState> {
        let mut uploaded: BTreeMap<String, ArtifactRef> = BTreeMap::new();
        for (name, path) in artifact_files {
            uploaded.insert(name.clone(), self.artifacts.put_file(path)?);
        }
        let inspection = self
            .state_access()
            .inspect_bundles(attempt_id, record_bytes)?;
        self.ingest_stored(attempt_id, worker_id, record_bytes, uploaded, inspection)
    }

    /// Like [`Self::ingest`], for artifacts that were already stored (the HTTP upload path).
    pub fn ingest_stored(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        record_bytes: &[u8],
        uploaded: BTreeMap<String, ArtifactRef>,
        inspection: SeriesInspection,
    ) -> Result<AttemptState> {
        let attempt = self.db.get_attempt(attempt_id)?;
        self.state_access()
            .require_live_lease(&attempt, chrono::Utc::now())?;
        for (name, artifact) in &uploaded {
            if !self.artifacts.contains(artifact) {
                return Err(OrchestratorError::Invalid(format!(
                    "artifact {name} ({}) was not uploaded",
                    artifact.sha256
                )));
            }
        }
        let (record_bytes, uploaded) = use_controller_patches(record_bytes, uploaded, &inspection)?;
        let record_bytes = record_bytes.as_slice();
        let record_ref = self.artifacts.put(record_bytes)?;

        let record: std::result::Result<AttemptRecord, _> = serde_json::from_slice(record_bytes);
        let (result_ref, outcome) = match &record {
            Ok(record) => {
                let result_ref = record.result.as_ref().map(|r| r.document.clone());
                let outcome = result_ref
                    .as_ref()
                    .and_then(|r| self.artifacts.get_json::<AgentResult>(r).ok())
                    .map(|r| r.outcome);
                (result_ref, outcome)
            }
            Err(_) => (None, None),
        };
        self.db.record_upload(
            attempt_id,
            worker_id,
            &record_ref,
            result_ref.as_ref(),
            outcome,
        )?;

        let job = self.load_job(&attempt)?;
        let mut problems = vec![];
        let mut parsed: Option<(AttemptRecord, AgentResult)> = None;
        let mut transient_failure = None;
        match record {
            Ok(record) => {
                let (record_problems, result) =
                    self.validate_record(&attempt, &job, &record, &uploaded, &inspection)?;
                problems.extend(record_problems);
                transient_failure =
                    crate::recovery::transient_failure(&self.artifacts, &job, &record, &problems);
                if let Some(result) = result {
                    parsed = Some((record, result));
                }
            }
            Err(e) => problems.push(format!("attempt record does not parse: {e}")),
        }

        let state = self.db.judge_attempt(attempt_id, problems.clone())?;
        let task = self.db.get_task(&attempt.task_id)?;
        match (state, parsed) {
            (AttemptState::Valid, Some((record, result))) => {
                if let Err(error) = self.apply_outcome(&task, &attempt, &job, &record, &result) {
                    // The upload has already been judged and cannot be retried. Preserve any
                    // published proposals and make partial failure actionable instead of leaving
                    // the task working forever with no active attempt.
                    self.needs_human(
                        &task,
                        &attempt,
                        &format!("could not apply accepted attempt outcome: {error}"),
                    )?;
                }
            }
            _ => {
                if let Some(reason) = transient_failure
                    && self
                        .config
                        .worker_pools
                        .get(&attempt.worker_pool)
                        .is_some_and(|pool| pool.transient_failure_retries > 0)
                    && self.retry_transient_execution(&task, &attempt, reason, &problems)?
                {
                    self.persist_forge_backoffs()?;
                    return Ok(state);
                }
                self.pause_task_source(
                    &task,
                    "The worker result was rejected; inspect it before resuming",
                )?;
                self.db.record_event(
                    Some(&task.id),
                    Some(attempt_id),
                    "orchestrator",
                    "attempt.rejected",
                    serde_json::json!({"problems": problems}),
                )?;
                if task.state != TaskState::NeedsHuman && !task.state.is_terminal() {
                    self.db
                        .set_task_state(&task.id, TaskState::NeedsHuman, "orchestrator")?;
                }
            }
        }
        self.persist_forge_backoffs()?;
        Ok(state)
    }

    fn validate_record(
        &self,
        attempt: &Attempt,
        job: &Job,
        record: &AttemptRecord,
        uploaded: &BTreeMap<String, ArtifactRef>,
        inspection: &SeriesInspection,
    ) -> Result<(Vec<String>, Option<AgentResult>)> {
        let mut problems = vec![];
        if record.schema_version != SCHEMA_VERSION {
            problems.push(format!(
                "attempt record schema_version {} is unsupported",
                record.schema_version
            ));
        }
        if record.attempt_id.as_str() != attempt.id {
            problems.push("attempt record names a different attempt".into());
        }
        if record.job != attempt.job {
            problems.push("attempt record answers a different job".into());
        }
        for entry in &record.artifacts {
            let expected = ArtifactRef {
                sha256: entry.sha256.clone(),
                size: entry.size,
            };
            match uploaded.get(&entry.name) {
                Some(actual) if actual == &expected => {}
                Some(_) => problems.push(format!(
                    "artifact {} differs from the uploaded file",
                    entry.name
                )),
                None if self.artifacts.contains(&expected) => {}
                None => problems.push(format!("artifact {} was not uploaded", entry.name)),
            }
        }
        if record.status != AttemptStatus::Completed {
            problems.push(format!("harness did not complete: {:?}", record.status));
        }

        // Result document.
        let mut result = None;
        match &record.result {
            None => problems.push("no result document".into()),
            Some(r) => match self.artifacts.get(&r.document) {
                Err(_) => problems.push("result document artifact is missing".into()),
                Ok(bytes) => {
                    if bytes.len() as u64 > job.limits.result_json_max_bytes {
                        problems.push("result document exceeds the size limit".into());
                    }
                    match serde_json::from_slice::<AgentResult>(&bytes) {
                        Ok(parsed) => {
                            problems.extend(validate_result(job, &parsed).into_iter().map(|p| p.0));
                            result = Some(parsed);
                        }
                        Err(e) => problems.push(format!("result document does not parse: {e}")),
                    }
                }
            },
        }

        // Repositories.
        let expected: BTreeSet<&RepositoryId> = job.repositories.iter().map(|r| &r.id).collect();
        let mut seen = BTreeSet::new();
        for collection in &record.repositories {
            if !seen.insert(&collection.id) {
                problems.push(format!("repository {} is reported twice", collection.id));
                continue;
            }
            let Some(job_repo) = job.repository(&collection.id) else {
                problems.push(format!(
                    "repository {} is not part of the job",
                    collection.id
                ));
                continue;
            };
            if collection.base_commit != job_repo.base_commit {
                problems.push(format!(
                    "repository {}: base commit differs from the job",
                    collection.id
                ));
            }
            if job_repo.access == RepositoryAccess::ReadOnly {
                if collection.read_only_violation {
                    problems.push(format!(
                        "repository {} is read_only but was modified",
                        collection.id
                    ));
                }
                continue;
            }
            if !collection.on_configured_branch {
                problems.push(format!(
                    "repository {}: HEAD is not on the configured branch",
                    collection.id
                ));
            }
            let Some(series) = &collection.series else {
                problems.push(format!("repository {}: no series collected", collection.id));
                continue;
            };
            let reported = result
                .as_ref()
                .and_then(|r| r.repositories.iter().find(|e| e.id == collection.id));
            let changed = reported.is_some_and(|r| r.changed);
            if changed {
                if !series.valid {
                    problems.push(format!(
                        "repository {}: series invalid: {}",
                        collection.id,
                        series.problems.join("; ")
                    ));
                    continue;
                }
                if series.commits.is_empty() {
                    problems.push(format!(
                        "repository {}: reported changed but has no commits",
                        collection.id
                    ));
                    continue;
                }
                // A proposal whose own verification failed is not worth publishing (decided
                // 2026-09-11): the attempt is invalid and a human sees the logs in the audit trail.
                if !job_repo.verification_recipe.is_empty() && collection.verification.is_empty() {
                    problems.push(format!(
                        "repository {}: verification recipe did not run",
                        collection.id
                    ));
                }
                if collection.verification.len() != job_repo.verification_recipe.len() {
                    problems.push(format!(
                        "repository {}: expected {} verification records, received {}",
                        collection.id,
                        job_repo.verification_recipe.len(),
                        collection.verification.len(),
                    ));
                }
                // Check the complete recipe against the orchestrator's frozen policy. This
                // establishes record consistency, not proof of execution on a compromised VM.
                for (expected, actual) in job_repo
                    .verification_recipe
                    .iter()
                    .zip(&collection.verification)
                {
                    if expected.name != actual.name
                        || expected.command != actual.command
                        || expected.cwd != actual.cwd
                    {
                        problems.push(format!(
                            "repository {}: verification record does not match configured step {}",
                            collection.id, expected.name,
                        ));
                    }
                }
                for step in &collection.verification {
                    let failure = match (step.skipped, step.timed_out, step.exit_code) {
                        (true, _, _) => Some("skipped".to_owned()),
                        (_, true, _) => Some("timed out".to_owned()),
                        (_, _, Some(0)) => None,
                        (_, _, Some(code)) => Some(format!("exit code {code}")),
                        (_, _, None) => Some("did not run".to_owned()),
                    };
                    if let Some(failure) = failure {
                        problems.push(format!(
                            "repository {}: verification step {} failed: {failure}",
                            collection.id, step.name
                        ));
                    }
                }
                let last = &series.commits[series.commits.len() - 1].sha;
                if last != &collection.head_commit
                    || reported.and_then(|r| r.head_commit.as_ref()) != Some(last)
                {
                    problems.push(format!(
                        "repository {}: head commit mismatch between result, series and checkout",
                        collection.id
                    ));
                    continue;
                }
                if series.bundle.is_none() {
                    problems.push(format!(
                        "repository {}: series has no bundle",
                        collection.id
                    ));
                    continue;
                }
                match inspection.problems.get(collection.id.as_str()) {
                    Some(found) => problems.extend(found.iter().cloned()),
                    None => problems.push(format!(
                        "repository {}: series was not inspected",
                        collection.id
                    )),
                }
            } else if !series.commits.is_empty() || collection.head_commit != job_repo.base_commit {
                problems.push(format!(
                    "repository {}: reported unchanged but HEAD moved",
                    collection.id
                ));
            }
        }
        for missing in expected.difference(&seen) {
            problems.push(format!("repository {missing} is missing from the record"));
        }

        Ok((problems, result))
    }
}

/// The Git work on a worker's bundles. It can be slow on hostile input, so it runs before ingest
/// instead of under the operation lock.
#[derive(Debug, Default)]
pub struct SeriesInspection {
    problems: BTreeMap<String, Vec<String>>,
    patches: BTreeMap<String, ArtifactRef>,
}

impl StateAccess {
    /// Fetches and checks every returned series, and builds each patch from the Git objects.
    pub fn inspect_bundles(
        &self,
        attempt_id: &str,
        record_bytes: &[u8],
    ) -> Result<SeriesInspection> {
        let mut inspection = SeriesInspection::default();
        let Ok(record) = serde_json::from_slice::<AttemptRecord>(record_bytes) else {
            return Ok(inspection);
        };
        let attempt = self.db.get_attempt(attempt_id)?;
        let job = self.load_job(&attempt)?;
        let timeout = u64::from(self.config.instance.limits.git_fetch_timeout_seconds);
        let deadline = Instant::now() + std::time::Duration::from_secs(timeout);
        for collection in &record.repositories {
            let job_repo = job.repositories.iter().find(|r| r.id == collection.id);
            let Some((job_repo, series)) = job_repo.zip(collection.series.as_ref()) else {
                continue;
            };
            let Some(bundle_ref) = series
                .bundle
                .as_ref()
                .filter(|_| !series.commits.is_empty())
            else {
                continue;
            };
            let (problems, patch) =
                self.inspect_series(&attempt, &job, job_repo, series, bundle_ref, deadline)?;
            let id = collection.id.to_string();
            inspection
                .problems
                .entry(id.clone())
                .or_default()
                .extend(problems);
            if let Some(patch) = patch {
                inspection.patches.insert(id, patch);
            }
        }
        Ok(inspection)
    }

    /// Fetches the series bundle into the trusted cache and re-checks the chain and content.
    fn inspect_series(
        &self,
        attempt: &Attempt,
        job: &Job,
        job_repo: &artidezi_contract::job::JobRepository,
        series: &artidezi_contract::attempt::SeriesCollection,
        bundle_ref: &ArtifactRef,
        deadline: Instant,
    ) -> Result<(Vec<String>, Option<ArtifactRef>)> {
        let mut problems = vec![];
        if series
            .commits
            .iter()
            .any(|commit| Oid::from_str(commit.sha.as_str()).is_err())
        {
            problems.push(format!("repository {}: malformed commit id", job_repo.id));
            return Ok((problems, None));
        }
        let task = self.db.get_task(&attempt.task_id)?;
        let policy: ResolvedProjectPolicy = serde_json::from_str(&task.policy_snapshot)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let resolved = &policy.repositories[job_repo.id.as_str()];
        let cache = crate::open_cache(&self.caches_root, &resolved.forge, &resolved.remote)?;
        let limits = &self.config.instance.limits;
        if bundle_ref.size > limits.max_bundle_bytes {
            problems.push(format!(
                "repository {}: bundle has {} bytes, above the {} byte limit",
                job_repo.id, bundle_ref.size, limits.max_bundle_bytes
            ));
            return Ok((problems, None));
        }
        let Ok(bundle_path) = self.artifacts.path(bundle_ref) else {
            problems.push(format!("repository {}: bundle is missing", job_repo.id));
            return Ok((problems, None));
        };
        let refname = format!("refs/artidezi/attempts/{}/{}", attempt.id, job_repo.id);
        let remaining = || Some(deadline.saturating_duration_since(Instant::now()));
        if let Err(e) = bundle::verify(cache.path(), &bundle_path, remaining()) {
            problems.push(format!(
                "repository {}: bundle does not verify: {e}",
                job_repo.id
            ));
            return Ok((problems, None));
        }
        let head = &series.commits[series.commits.len() - 1].sha;
        if let Err(e) = bundle::fetch(
            cache.path(),
            &bundle_path,
            &[&format!("+{head}:{refname}")],
            remaining(),
        ) {
            problems.push(format!(
                "repository {}: cannot fetch bundle: {e}",
                job_repo.id
            ));
            return Ok((problems, None));
        }
        if series.commits.len() as u32 > job.limits.max_commits_per_repository {
            problems.push(format!("repository {}: too many commits", job_repo.id));
        }
        let mut oids = vec![];
        for commit in &series.commits {
            if commit.message.len() as u64 > job.limits.max_commit_message_bytes {
                problems.push(format!(
                    "repository {}: commit message too long",
                    job_repo.id
                ));
            }
            match Oid::from_str(commit.sha.as_str()) {
                Ok(oid) => oids.push(oid),
                Err(_) => problems.push(format!("repository {}: malformed commit id", job_repo.id)),
            }
        }
        let base = Oid::from_str(job_repo.base_commit.as_str())
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let rules = ContentRules {
            allowed_paths: job_repo.allowed_paths.clone(),
            max_file_bytes: job.limits.max_diff_bytes_per_repository,
        };
        // The record's messages become PR titles, so they must match what gets published.
        for (commit, oid) in series.commits.iter().zip(&oids) {
            let matches = cache
                .repo()
                .find_commit(*oid)
                .is_ok_and(|object| object.message_raw().unwrap_or("") == commit.message);
            if !matches {
                problems.push(format!(
                    "repository {}: commit {oid} message differs from the record",
                    job_repo.id
                ));
            }
        }
        problems.extend(
            inspect(cache.repo(), base, &oids, &rules)?
                .into_iter()
                .map(|p| format!("repository {}: {p}", job_repo.id)),
        );
        if !problems.is_empty() {
            return Ok((problems, None));
        }
        let head =
            Oid::from_str(head.as_str()).map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        let patch = artidezi_git::series::patch(cache.repo(), base, head)?;
        if patch.len() as u64 > job.limits.max_diff_bytes_per_repository {
            problems.push(format!(
                "repository {}: diff is above the {} byte limit",
                job_repo.id, job.limits.max_diff_bytes_per_repository
            ));
            return Ok((problems, None));
        }
        let _guard = self.artifacts.upload_guard();
        let artifact = ArtifactRef {
            sha256: hex::encode(Sha256::digest(&patch)),
            size: patch.len() as u64,
        };
        self.db
            .pin_artifact_upload(&attempt.id, &artifact, chrono::Utc::now())?;
        Ok((problems, Some(self.artifacts.put(&patch)?)))
    }
}

/// Reviewers see the patch built from the Git objects, not the one the worker claims.
fn use_controller_patches(
    record_bytes: &[u8],
    mut uploaded: BTreeMap<String, ArtifactRef>,
    inspection: &SeriesInspection,
) -> Result<(Vec<u8>, BTreeMap<String, ArtifactRef>)> {
    let Ok(mut record) = serde_json::from_slice::<AttemptRecord>(record_bytes) else {
        return Ok((record_bytes.to_vec(), uploaded));
    };
    for collection in &mut record.repositories {
        let patch = inspection.patches.get(collection.id.as_str());
        let Some((patch, series)) = patch.zip(collection.series.as_mut()) else {
            continue;
        };
        let previous = series.diff.replace(patch.clone());
        let entry = record
            .artifacts
            .iter_mut()
            .find(|entry| previous.as_ref().is_some_and(|p| p.sha256 == entry.sha256));
        if let Some(entry) = entry {
            entry.sha256.clone_from(&patch.sha256);
            entry.size = patch.size;
            uploaded.insert(entry.name.clone(), patch.clone());
        }
    }
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
    Ok((bytes, uploaded))
}

/// Every artifact a job references, so a worker may download exactly those and nothing else.
pub fn job_artifacts(job: &Job) -> Vec<ArtifactRef> {
    let mut refs = vec![job.task_document.clone()];
    for repo in &job.repositories {
        refs.push(repo.snapshot_bundle.clone());
        refs.extend(repo.input_refs.iter().map(|i| i.bundle.clone()));
    }
    if let Some(prev) = &job.previous_attempt {
        refs.push(prev.attempt_document.clone());
        refs.extend(prev.result_document.clone());
        refs.extend(prev.commit_bundles.values().cloned());
    }
    refs
}
