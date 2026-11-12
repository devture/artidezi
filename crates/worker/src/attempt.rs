// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use artidezi_contract::attempt::*;
use artidezi_contract::common::{
    ArtifactRef, CommitSha, Outcome, RepositoryAccess, RepositoryId, SCHEMA_VERSION,
};
use artidezi_contract::job::{Harness, JobRepository, RecipeStep};
use artidezi_contract::validate::validate_result;
use artidezi_contract::{AgentResult, Job};
use artidezi_git::bundle;
use artidezi_git::series::{CollectLimits, collect};
use artidezi_git::workspace::{InputRefSpec, prepare_checkout};
use artidezi_harness::codex::prework_auth_rejection;
use artidezi_harness::{HarnessAdapter, InvocationContext};
use chrono::Utc;
use git2::{Oid, Repository};
use sha2::{Digest, Sha256};

use crate::credentials;
use crate::process::{self, Spec};
use crate::{Cancellation, Result, WorkerError, io_err};

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub worker_pool: String,
    pub worker_id: String,
    /// Each attempt gets `<workspaces_root>/<attempt_id>/`.
    pub workspaces_root: PathBuf,
    /// `<credentials_root>/<credential_ref>/` holds harness credential material, if any.
    pub credentials_root: Option<PathBuf>,
    /// Logs above this size are truncated (head kept) before upload.
    pub max_log_bytes: u64,
}

pub struct PreviousInputs {
    pub attempt_document: Vec<u8>,
    pub result_document: Option<Vec<u8>>,
    /// Series bundles of the previous attempt, by repository id.
    pub commit_bundles: BTreeMap<RepositoryId, PathBuf>,
}

pub struct AttemptInputs<'a> {
    pub job: &'a Job,
    /// The exact bytes of `job.json`, so the record can reference the same hash.
    pub job_bytes: &'a [u8],
    pub task_document: &'a [u8],
    /// Snapshot bundle per repository id.
    pub snapshot_bundles: BTreeMap<RepositoryId, PathBuf>,
    /// Input-ref bundles keyed by (repository id, ref name).
    pub input_ref_bundles: BTreeMap<(RepositoryId, String), PathBuf>,
    pub previous: Option<PreviousInputs>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedArtifact {
    pub name: String,
    pub media_type: String,
    pub path: PathBuf,
    pub artifact: ArtifactRef,
}

pub struct AttemptOutput {
    pub record: AttemptRecord,
    /// Files the supervisor uploads before the record. The record only references their hashes.
    pub artifacts: Vec<StagedArtifact>,
    pub workspace: PathBuf,
}

impl AttemptOutput {
    /// Destroys all attempt-local state. Called after the orchestrator acknowledged the upload.
    pub fn cleanup(&self) -> Result<()> {
        fs::remove_dir_all(&self.workspace).map_err(io_err(&self.workspace))
    }
}

struct Staging<'a> {
    out_dir: PathBuf,
    max_log_bytes: u64,
    artifacts: &'a mut Vec<StagedArtifact>,
}

impl Staging<'_> {
    fn stage_file(
        &mut self,
        name: &str,
        media_type: &str,
        path: &Path,
        truncate: bool,
    ) -> Result<(ArtifactRef, bool)> {
        let mut truncated = false;
        let staged_path = self.out_dir.join(name);
        if truncate {
            let len = fs::metadata(path).map_err(io_err(path))?.len();
            if len > self.max_log_bytes {
                let mut file = fs::File::open(path).map_err(io_err(path))?;
                let mut head = vec![0u8; self.max_log_bytes as usize];
                file.read_exact(&mut head).map_err(io_err(path))?;
                head.extend_from_slice(b"\n[truncated by artidezi worker]\n");
                fs::write(&staged_path, &head).map_err(io_err(&staged_path))?;
                truncated = true;
            }
        }
        if !truncated && staged_path != path {
            fs::copy(path, &staged_path).map_err(io_err(path))?;
        }
        let artifact = digest_file(&staged_path)?;
        self.artifacts.push(StagedArtifact {
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            path: staged_path,
            artifact: artifact.clone(),
        });
        Ok((artifact, truncated))
    }

    fn stage_bytes(&mut self, name: &str, media_type: &str, bytes: &[u8]) -> Result<ArtifactRef> {
        let staged_path = self.out_dir.join(name);
        fs::write(&staged_path, bytes).map_err(io_err(&staged_path))?;
        let artifact = ArtifactRef {
            sha256: hex::encode(Sha256::digest(bytes)),
            size: bytes.len() as u64,
        };
        self.artifacts.push(StagedArtifact {
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            path: staged_path,
            artifact: artifact.clone(),
        });
        Ok(artifact)
    }
}

fn digest_file(path: &Path) -> Result<ArtifactRef> {
    let mut file = fs::File::open(path).map_err(io_err(path))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(io_err(path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok(ArtifactRef {
        sha256: hex::encode(hasher.finalize()),
        size,
    })
}

fn wait_before_auth_retry(delay: Duration, cancellation: &Cancellation) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < delay {
        cancellation.check()?;
        std::thread::sleep((delay - started.elapsed()).min(Duration::from_millis(100)));
    }
    cancellation.check()
}

fn run_recipe(
    steps: &[RecipeStep],
    repo_dir: &Path,
    env: &[(String, String)],
    staging: &mut Staging<'_>,
    prefix: &str,
    cancellation: &Cancellation,
) -> Result<(Vec<VerificationStep>, bool)> {
    let mut records = vec![];
    let mut failed = false;
    for (index, step) in steps.iter().enumerate() {
        cancellation.check()?;
        let mut record = VerificationStep {
            name: step.name.clone(),
            command: step.command.clone(),
            cwd: step.cwd.clone(),
            exit_code: None,
            started_at: None,
            finished_at: None,
            stdout: None,
            stderr: None,
            truncated: false,
            timed_out: false,
            skipped: failed,
        };
        if !failed {
            let stdout_path = staging.out_dir.join(format!("{prefix}-{index}-stdout.log"));
            let stderr_path = staging.out_dir.join(format!("{prefix}-{index}-stderr.log"));
            let program = PathBuf::from(&step.command[0]);
            let outcome = process::run_cancellable(
                &Spec {
                    program: &program,
                    args: &step.command[1..],
                    env,
                    cwd: &repo_dir.join(&step.cwd),
                    stdin: None,
                    timeout: Duration::from_secs(step.timeout_seconds),
                    stdout_path: &stdout_path,
                    stderr_path: &stderr_path,
                },
                cancellation,
            )?;
            let (stdout, t1) = staging.stage_file(
                &format!("{prefix}-{index}-stdout.log"),
                "text/plain",
                &stdout_path,
                true,
            )?;
            let (stderr, t2) = staging.stage_file(
                &format!("{prefix}-{index}-stderr.log"),
                "text/plain",
                &stderr_path,
                true,
            )?;
            record.exit_code = outcome.exit_code;
            record.started_at = Some(outcome.started_at);
            record.finished_at = Some(outcome.finished_at);
            record.stdout = Some(stdout);
            record.stderr = Some(stderr);
            record.truncated = t1 || t2;
            record.timed_out = outcome.timed_out;
            if outcome.exit_code != Some(0) {
                failed = true;
            }
        }
        records.push(record);
    }
    Ok((records, !failed))
}

fn write_previous(task_dir: &Path, previous: &PreviousInputs) -> Result<()> {
    let dir = task_dir.join("previous");
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    fs::write(dir.join("attempt.json"), &previous.attempt_document).map_err(io_err(&dir))?;
    if let Some(result) = &previous.result_document {
        fs::write(dir.join("result.json"), result).map_err(io_err(&dir))?;
    }
    for (repo_id, bundle_path) in &previous.commit_bundles {
        let repo_dir = dir.join(repo_id.as_str());
        fs::create_dir_all(&repo_dir).map_err(io_err(&repo_dir))?;
        fs::copy(bundle_path, repo_dir.join("commits.bundle")).map_err(io_err(bundle_path))?;
    }
    Ok(())
}

fn collect_repository(
    repo: &JobRepository,
    repo_dir: &Path,
    job: &Job,
    staging: &mut Staging<'_>,
) -> Result<RepositoryCollection> {
    let base = Oid::from_str(repo.base_commit.as_str())
        .map_err(|e| WorkerError::Invalid(e.to_string()))?;
    let limits = CollectLimits {
        max_commits: job.limits.max_commits_per_repository,
        max_commit_message_bytes: job.limits.max_commit_message_bytes,
    };
    let collected = collect(repo_dir, &repo.branch, base, limits)?;
    let mut record = RepositoryCollection {
        id: repo.id.clone(),
        base_commit: repo.base_commit.clone(),
        head_commit: CommitSha(collected.head.to_string()),
        on_configured_branch: collected.on_configured_branch,
        series: None,
        dirty_tracked_files: collected.dirty_tracked_files.clone(),
        untracked_files_count: collected.untracked_files_count,
        read_only_violation: false,
        setup: vec![],
        verification: vec![],
    };

    if repo.access == RepositoryAccess::ReadOnly {
        record.read_only_violation =
            collected.head != base || !collected.dirty_tracked_files.is_empty();
        return Ok(record);
    }

    let mut series = SeriesCollection {
        valid: collected.is_valid(),
        oversized: false,
        commits: collected
            .commits
            .iter()
            .map(|c| CollectedCommit {
                sha: CommitSha(c.oid.to_string()),
                parent: CommitSha(c.parent.to_string()),
                message: c.message.clone(),
                author: c.author.clone(),
                files_changed: c.files_changed,
                insertions: c.insertions,
                deletions: c.deletions,
            })
            .collect(),
        bundle: None,
        diff: None,
        problems: collected.problems.clone(),
    };
    if collected.problems.iter().any(|p| p.contains("exceeds")) {
        series.oversized = true;
    }

    if series.valid && !series.commits.is_empty() {
        let git_repo = Repository::open(repo_dir)?;
        let patch = artidezi_git::series::patch(&git_repo, base, collected.head)?;
        if patch.len() as u64 > job.limits.max_diff_bytes_per_repository {
            series.valid = false;
            series.oversized = true;
            series.problems.push(format!(
                "diff is {} bytes, above the {} byte limit",
                patch.len(),
                job.limits.max_diff_bytes_per_repository
            ));
        } else {
            let bundle_path = staging.out_dir.join(format!("{}-commits.bundle", repo.id));
            bundle::create(
                repo_dir,
                &bundle_path,
                &[&format!("{base}..refs/heads/{}", repo.branch)],
            )?;
            let (bundle_ref, _) = staging.stage_file(
                &format!("{}-commits.bundle", repo.id),
                "application/x-git-bundle",
                &bundle_path,
                false,
            )?;
            series.bundle = Some(bundle_ref);
            series.diff = Some(staging.stage_bytes(
                &format!("{}-diff.patch", repo.id),
                "text/x-diff",
                &patch,
            )?);
        }
    }
    record.series = Some(series);
    Ok(record)
}

/// Runs one attempt from prepared inputs to a staged attempt record. Never returns an error for
/// anything the agent did; those become facts in the record. Errors are reserved for the
/// supervisor's own failures (disk, git plumbing, missing inputs).
pub fn run_attempt(
    config: &WorkerConfig,
    harness: &dyn HarnessAdapter,
    inputs: &AttemptInputs<'_>,
) -> Result<AttemptOutput> {
    run_attempt_cancellable(config, harness, inputs, &Cancellation::default())
}

/// A revoked attempt keeps its workspace for inspection and never runs subsequent stages.
pub fn run_attempt_cancellable(
    config: &WorkerConfig,
    harness: &dyn HarnessAdapter,
    inputs: &AttemptInputs<'_>,
    cancellation: &Cancellation,
) -> Result<AttemptOutput> {
    cancellation.check()?;
    let job = inputs.job;
    let workspace = config.workspaces_root.join(job.attempt_id.as_str());
    if workspace.exists() {
        return Err(WorkerError::Invalid(format!(
            "workspace {} already exists",
            workspace.display()
        )));
    }
    let task_dir = workspace.join("task");
    let repos_dir = workspace.join("repos");
    let out_dir = workspace.join("out");
    let home_dir = workspace.join("home");
    // Scratch space on the workspaces disk instead of the host's /tmp, which may be a small tmpfs.
    let tmp_dir = workspace.join("tmp");
    for dir in [&task_dir, &repos_dir, &out_dir, &home_dir, &tmp_dir] {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    // Logs can echo secrets, home holds credential copies, and tools write anything to tmp.
    for dir in [&out_dir, &home_dir, &tmp_dir] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err(dir))?;
    }
    let task_document = task_dir.join("task.md");
    fs::write(&task_document, inputs.task_document).map_err(io_err(&task_document))?;
    fs::write(task_dir.join("job.json"), inputs.job_bytes).map_err(io_err(&task_dir))?;
    if let Some(previous) = &inputs.previous {
        write_previous(&task_dir, previous)?;
    }

    let mut artifacts = vec![];
    let mut staging = Staging {
        out_dir: out_dir.clone(),
        max_log_bytes: config.max_log_bytes,
        artifacts: &mut artifacts,
    };
    let job_ref = ArtifactRef {
        sha256: hex::encode(Sha256::digest(inputs.job_bytes)),
        size: inputs.job_bytes.len() as u64,
    };
    let mut record = AttemptRecord {
        schema_version: SCHEMA_VERSION,
        attempt_id: job.attempt_id.clone(),
        job: job_ref,
        status: AttemptStatus::Completed,
        supervisor: SupervisorInfo {
            worker_pool: config.worker_pool.clone(),
            worker_id: config.worker_id.clone(),
            version: artidezi_contract::app_version().to_owned(),
        },
        harness: HarnessRecord {
            name: format!("{:?}", harness.harness()).to_lowercase(),
            version: harness.installed_version()?,
            model: None,
            exit_code: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            session_id: None,
            usage: None,
            transcript: None,
            stdout: None,
            stderr: None,
        },
        result: None,
        repositories: vec![],
        artifacts: vec![],
    };

    // Checkouts.
    let mut repo_dirs: Vec<(usize, PathBuf)> = vec![];
    for (index, repo) in job.repositories.iter().enumerate() {
        cancellation.check()?;
        let snapshot = inputs.snapshot_bundles.get(&repo.id).ok_or_else(|| {
            WorkerError::Invalid(format!("no snapshot bundle for repository {}", repo.id))
        })?;
        let repo_dir = repos_dir.join(repo.id.as_str());
        let mut input_specs = vec![];
        for input in &repo.input_refs {
            let bundle_path = inputs
                .input_ref_bundles
                .get(&(repo.id.clone(), input.name.clone()))
                .ok_or_else(|| {
                    WorkerError::Invalid(format!(
                        "no bundle for input ref {} of {}",
                        input.name, repo.id
                    ))
                })?;
            input_specs.push(InputRefSpec {
                name: &input.name,
                commit: Oid::from_str(input.commit.as_str())
                    .map_err(|e| WorkerError::Invalid(e.to_string()))?,
                bundle_path,
            });
        }
        let base = Oid::from_str(repo.base_commit.as_str())
            .map_err(|e| WorkerError::Invalid(e.to_string()))?;
        prepare_checkout(snapshot, &repo_dir, &repo.branch, base, &input_specs)?;
        repo_dirs.push((index, repo_dir));
    }
    let primary_index = job
        .repositories
        .iter()
        .position(|r| r.access == RepositoryAccess::ReadWrite)
        .unwrap_or(0);
    let primary_repo_dir = repos_dir.join(job.repositories[primary_index].id.as_str());

    let credential_dir = config
        .credentials_root
        .as_ref()
        .map(|root| root.join(&job.agent_profile.model_endpoint.credential_ref))
        .filter(|dir| dir.is_dir());
    let result_path = out_dir.join("result.json");
    let ctx = InvocationContext {
        job,
        workspace: &workspace,
        task_document: &task_document,
        result_path: &result_path,
        primary_repo_dir: &primary_repo_dir,
        home: &home_dir,
        tmp: &tmp_dir,
        credential_dir: credential_dir.as_deref(),
    };
    let common_env = artidezi_harness::common_env(&ctx);

    // Setup recipes run before the harness with the same environment; a failure aborts the
    // attempt as a harness error because the agent never got a usable workspace.
    let mut setup_records: BTreeMap<RepositoryId, Vec<VerificationStep>> = BTreeMap::new();
    let mut setup_ok = true;
    for (index, repo_dir) in &repo_dirs {
        cancellation.check()?;
        let repo = &job.repositories[*index];
        if repo.setup_recipe.is_empty() {
            continue;
        }
        let (records, ok) = run_recipe(
            &repo.setup_recipe,
            repo_dir,
            &common_env,
            &mut staging,
            &format!("setup-{}", repo.id),
            cancellation,
        )?;
        setup_records.insert(repo.id.clone(), records);
        setup_ok &= ok;
    }

    let mut facts = artidezi_harness::TranscriptFacts::default();
    if setup_ok {
        cancellation.check()?;
        let invocation = harness.build_invocation(&ctx)?;
        let credential_updates = credentials::prepare(&invocation.credential_updates)?;
        let stdout_path = out_dir.join("harness-stdout.log");
        let stderr_path = out_dir.join("harness-stderr.log");
        // Codex may refresh its attempt-local login only as a failed 401 turn ends. Retry that
        // exact pre-work failure at most twice, within the same wall-clock ceiling. Preserve each
        // failed stream as evidence; an unknown event or any agent work disables replay.
        let harness_clock = Instant::now();
        let harness_limit = Duration::from_secs(job.limits.wall_clock_seconds);
        let retry_delays = [Duration::from_secs(5), Duration::from_secs(20)];
        let mut prior_logs = Vec::new();
        let mut first_started_at = None;
        let outcome = loop {
            let remaining = harness_limit.saturating_sub(harness_clock.elapsed());
            let run = process::run_cancellable(
                &Spec {
                    program: &invocation.program,
                    args: &invocation.args,
                    env: &invocation.env,
                    cwd: &invocation.cwd,
                    stdin: invocation.stdin.as_deref(),
                    timeout: remaining,
                    stdout_path: &stdout_path,
                    stderr_path: &stderr_path,
                },
                cancellation,
            );
            let current = match run {
                Ok(current) => current,
                Err(error) => break Err(error),
            };
            first_started_at.get_or_insert(current.started_at);
            let delay = retry_delays.get(prior_logs.len()).copied();
            let can_retry = delay.is_some_and(|delay| {
                harness_limit.saturating_sub(harness_clock.elapsed())
                    > delay + Duration::from_secs(1)
            }) && harness.harness() == Harness::Codex
                && current.exit_code == Some(1)
                && !current.timed_out
                && !result_path.exists()
                && fs::metadata(&stdout_path).is_ok_and(|metadata| metadata.len() <= 64 * 1024)
                && fs::read(&stdout_path).is_ok_and(|stdout| prework_auth_rejection(&stdout));
            if !can_retry {
                break Ok(current);
            }
            let retry_number = prior_logs.len() + 1;
            let retry_stdout =
                out_dir.join(format!("harness-auth-retry-{retry_number}-stdout.log"));
            let retry_stderr =
                out_dir.join(format!("harness-auth-retry-{retry_number}-stderr.log"));
            if let Err(error) =
                fs::rename(&stdout_path, &retry_stdout).map_err(io_err(&stdout_path))
            {
                break Err(error);
            }
            if let Err(error) =
                fs::rename(&stderr_path, &retry_stderr).map_err(io_err(&stderr_path))
            {
                break Err(error);
            }
            prior_logs.push((retry_stdout, retry_stderr));
            tracing::warn!(attempt = %job.attempt_id, retry_number, "retrying Codex after pre-work authentication rejection");
            if let Err(error) =
                wait_before_auth_retry(delay.expect("can_retry has a delay"), cancellation)
            {
                break Err(error);
            }
        };
        // A still-running process may still mutate its login; retain all evidence for recovery.
        if matches!(outcome, Err(WorkerError::UnstoppableProcess { .. })) {
            return Err(outcome.unwrap_err());
        }
        let credential_warnings = credentials::persist(&credential_updates);
        if !credential_warnings.is_empty() {
            for warning in &credential_warnings {
                tracing::error!(attempt = %job.attempt_id, warning, "credential update was not persisted");
            }
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_path)
            {
                Ok(mut stderr) => {
                    for warning in &credential_warnings {
                        let _ = writeln!(stderr, "[artidezi] {warning}");
                    }
                }
                Err(error) => tracing::error!(
                    attempt = %job.attempt_id,
                    %error,
                    "could not append the credential warning to harness stderr"
                ),
            }
        }
        let outcome = outcome?;
        record.harness.started_at = first_started_at.unwrap_or(outcome.started_at);
        record.harness.finished_at = outcome.finished_at;
        record.harness.exit_code = outcome.exit_code;
        record.status = if outcome.timed_out {
            AttemptStatus::Timeout
        } else if outcome.spawn_error.is_some() || outcome.exit_code.is_none() {
            AttemptStatus::HarnessError
        } else {
            AttemptStatus::Completed
        };
        let stdout_bytes = fs::read(&stdout_path).map_err(io_err(&stdout_path))?;
        facts = harness.read_transcript(&stdout_bytes);
        record.harness.session_id = facts.session_id.clone();
        record.harness.model = facts.model.clone();
        if facts.input_tokens.is_some() || facts.output_tokens.is_some() {
            record.harness.usage = Some(Usage {
                input_tokens: facts.input_tokens.unwrap_or(0),
                output_tokens: facts.output_tokens.unwrap_or(0),
            });
        }
        for (index, (stdout, stderr)) in prior_logs.iter().enumerate() {
            let number = index + 1;
            staging.stage_file(
                &format!("harness-auth-retry-{number}-stdout.log"),
                "application/x-ndjson",
                stdout,
                true,
            )?;
            staging.stage_file(
                &format!("harness-auth-retry-{number}-stderr.log"),
                "text/plain",
                stderr,
                true,
            )?;
        }
        let (transcript, _) = staging.stage_file(
            "harness-stdout.log",
            "application/x-ndjson",
            &stdout_path,
            true,
        )?;
        let (stderr, _) =
            staging.stage_file("harness-stderr.log", "text/plain", &stderr_path, true)?;
        record.harness.transcript = Some(transcript.clone());
        record.harness.stdout = Some(transcript);
        record.harness.stderr = Some(stderr);
    } else {
        record.status = AttemptStatus::HarnessError;
    }

    // Result document.
    cancellation.check()?;
    let mut parsed_result: Option<AgentResult> = None;
    let result_bytes = match fs::metadata(&result_path) {
        Ok(meta) if meta.len() <= job.limits.result_json_max_bytes => Some((
            ResultSource::File,
            fs::read(&result_path).map_err(io_err(&result_path))?,
        )),
        Ok(meta) => {
            record.result = Some(ResultRecord {
                source: ResultSource::File,
                document: staging.stage_bytes("result.json", "application/json", b"")?,
                validation_errors: vec![format!(
                    "result.json is {} bytes, above the {} byte limit",
                    meta.len(),
                    job.limits.result_json_max_bytes
                )],
            });
            None
        }
        Err(_) => facts.fallback_result.as_ref().map(|v| {
            (
                ResultSource::HarnessOutput,
                serde_json::to_vec_pretty(v).unwrap_or_default(),
            )
        }),
    };
    if let Some((source, bytes)) = result_bytes {
        let document = staging.stage_bytes("result.json", "application/json", &bytes)?;
        let mut validation_errors = vec![];
        match serde_json::from_slice::<AgentResult>(&bytes) {
            Ok(result) => {
                validation_errors.extend(validate_result(job, &result).into_iter().map(|p| p.0));
                parsed_result = Some(result);
            }
            Err(e) => validation_errors.push(format!("result.json does not parse: {e}")),
        }
        record.result = Some(ResultRecord {
            source,
            document,
            validation_errors,
        });
    }

    // Collection.
    for (index, repo_dir) in &repo_dirs {
        cancellation.check()?;
        let repo = &job.repositories[*index];
        let mut collection = collect_repository(repo, repo_dir, job, &mut staging)?;
        if let Some(setup) = setup_records.remove(&repo.id) {
            collection.setup = setup;
        }
        if let (Some(series), Some(result)) = (&mut collection.series, &parsed_result)
            && let Some(reported) = result.repositories.iter().find(|r| r.id == repo.id)
        {
            match (&reported.head_commit, reported.changed) {
                (Some(head), true) if head != &collection.head_commit => {
                    series.valid = false;
                    series.problems.push(format!(
                        "agent reported head {head} but the checkout is at {}",
                        collection.head_commit
                    ));
                }
                (_, false) if !series.commits.is_empty() => {
                    series.valid = false;
                    series
                        .problems
                        .push("agent reported no change but left commits".into());
                }
                _ => {}
            }
        }
        record.repositories.push(collection);
    }

    // Verification runs only for a valid, non-empty proposal series.
    let is_proposal = parsed_result
        .as_ref()
        .is_some_and(|r| r.outcome == Outcome::Proposal);
    if is_proposal {
        for (index, repo_dir) in &repo_dirs {
            cancellation.check()?;
            let repo = &job.repositories[*index];
            let collection = &mut record.repositories[*index];
            let runnable = collection
                .series
                .as_ref()
                .is_some_and(|s| s.valid && !s.commits.is_empty());
            if runnable && !repo.verification_recipe.is_empty() {
                let (records, _) = run_recipe(
                    &repo.verification_recipe,
                    repo_dir,
                    &common_env,
                    &mut staging,
                    &format!("verify-{}", repo.id),
                    cancellation,
                )?;
                collection.verification = records;
            }
        }
    }

    cancellation.check()?;
    record.artifacts = artifacts
        .iter()
        .map(|a| ArtifactEntry {
            name: a.name.clone(),
            sha256: a.artifact.sha256.clone(),
            size: a.artifact.size,
            media_type: a.media_type.clone(),
        })
        .collect();
    let record_path = out_dir.join("attempt.json");
    fs::write(
        &record_path,
        serde_json::to_vec_pretty(&record).map_err(|e| WorkerError::Invalid(e.to_string()))?,
    )
    .map_err(io_err(&record_path))?;

    Ok(AttemptOutput {
        record,
        artifacts,
        workspace,
    })
}
