// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Automatically selected, bounded GitHub Actions job evidence. This never follows check-run URLs
//! or fetches complete workflow archives. Missing optional evidence does not change the CI gate.

use chrono::Utc;
use serde::Deserialize;

use super::GithubForge;
use crate::ci_log::{LogCollector, MAX_SELECTED_JOBS, failure, unavailable};
use crate::{CiJobLog, CiLogOutcome};
use artidezi_contract::common::CommitSha;

const MAX_RUNS: usize = 20;
const MAX_JOBS_PER_RUN: usize = 100;

#[derive(Deserialize)]
struct RunPage {
    total_count: usize,
    workflow_runs: Vec<Run>,
}

#[derive(Clone, Deserialize)]
struct Run {
    id: u64,
    head_sha: String,
    run_attempt: u64,
    repository: Repo,
}

#[derive(Clone, Deserialize)]
struct Repo {
    full_name: String,
}

#[derive(Deserialize)]
struct JobPage {
    total_count: usize,
    jobs: Vec<Job>,
}

#[derive(Clone, Deserialize)]
struct Job {
    id: u64,
    run_id: u64,
    head_sha: String,
    name: String,
    status: String,
    conclusion: Option<String>,
}

pub(super) fn collect(
    forge: &GithubForge,
    remote: &str,
    head: &CommitSha,
    names: &[String],
) -> Vec<CiJobLog> {
    if names.is_empty() {
        return Vec::new();
    }
    let unavailable_names = |outcome| {
        names
            .iter()
            .take(MAX_SELECTED_JOBS)
            .map(|name| unavailable(name, outcome))
            .collect()
    };
    let token = match forge.actions_token(remote) {
        Ok(token) => token,
        Err(error) => return unavailable_names(failure(&error)),
    };
    let runs_path = format!(
        "/repos/{remote}/actions/runs?head_sha={}&per_page=100&page=1",
        head.as_str()
    );
    let page: RunPage = match forge.http().get(&runs_path, Some(&token), false) {
        Ok(page) => page,
        Err(error) => return unavailable_names(failure(&error)),
    };
    // A partial listing could silently choose an old attempt or omit a failure.
    if page.total_count > MAX_RUNS || page.workflow_runs.len() != page.total_count {
        return unavailable_names(CiLogOutcome::FetchFailed);
    }
    let mut jobs = Vec::new();
    for run in &page.workflow_runs {
        if run.head_sha != head.as_str()
            || !run.repository.full_name.eq_ignore_ascii_case(remote)
            || run.id == 0
            || run.run_attempt == 0
        {
            return unavailable_names(CiLogOutcome::FetchFailed);
        }
        let path = format!(
            "/repos/{remote}/actions/runs/{}/attempts/{}/jobs?per_page=100&page=1",
            run.id, run.run_attempt
        );
        let page: JobPage = match forge.http().get(&path, Some(&token), false) {
            Ok(page) => page,
            Err(error) => return unavailable_names(failure(&error)),
        };
        if page.total_count > MAX_JOBS_PER_RUN || page.jobs.len() != page.total_count {
            return unavailable_names(CiLogOutcome::FetchFailed);
        }
        for job in page.jobs {
            if job.run_id != run.id || job.head_sha != head.as_str() || job.id == 0 {
                return unavailable_names(CiLogOutcome::FetchFailed);
            }
            if names.contains(&job.name) {
                jobs.push((run.clone(), job));
            }
        }
    }
    // Keep the latest observed attempt for each failed check name. Completed failures take the
    // limited log slots before jobs that became pending or successful during a rerun.
    jobs.sort_by(|a, b| {
        a.1.name
            .cmp(&b.1.name)
            .then_with(|| b.0.id.cmp(&a.0.id))
            .then_with(|| b.1.id.cmp(&a.1.id))
    });
    jobs.dedup_by(|a, b| a.1.name == b.1.name);
    jobs.sort_by(|a, b| {
        let failed =
            |job: &Job| job.status == "completed" && job.conclusion.as_deref() == Some("failure");
        failed(&b.1)
            .cmp(&failed(&a.1))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    jobs.into_iter()
        .take(MAX_SELECTED_JOBS)
        .map(|(run, job)| {
            let mut result = unavailable(&job.name, CiLogOutcome::Pending);
            result.job_id = Some(job.id);
            result.run_id = Some(run.id);
            result.run_attempt = Some(run.run_attempt);
            if job.status != "completed" {
                return result;
            }
            if job.conclusion.as_deref() != Some("failure") {
                result.outcome = CiLogOutcome::MissingOrInaccessible;
                return result;
            }
            let job_path = format!("/repos/{remote}/actions/jobs/{}", job.id);
            let exact: Job = match forge.http().get(&job_path, Some(&token), false) {
                Ok(job) => job,
                Err(error) => {
                    result.outcome = failure(&error);
                    return result;
                }
            };
            if exact.id != job.id
                || exact.run_id != run.id
                || exact.head_sha != head.as_str()
                || exact.name != job.name
                || exact.status != "completed"
                || exact.conclusion.as_deref() != Some("failure")
            {
                result.outcome = CiLogOutcome::FetchFailed;
                return result;
            }
            let path = format!("/repos/{remote}/actions/jobs/{}/logs", job.id);
            let mut collector = LogCollector::default();
            let raw_bytes = match forge
                .http()
                .job_log(&path, &token, |chunk| collector.push(chunk))
            {
                Ok(bytes) => bytes,
                Err(outcome) => {
                    result.outcome = outcome;
                    return result;
                }
            };
            let run_path = format!("/repos/{remote}/actions/runs/{}", run.id);
            let current: Run = match forge.http().get(&run_path, Some(&token), false) {
                Ok(run) => run,
                Err(error) => {
                    result.outcome = failure(&error);
                    return result;
                }
            };
            if current.id != run.id
                || current.run_attempt != run.run_attempt
                || current.head_sha != head.as_str()
                || !current.repository.full_name.eq_ignore_ascii_case(remote)
            {
                result.outcome = CiLogOutcome::FetchFailed;
                return result;
            }
            let (excerpt, sanitized_bytes, truncated) = collector.finish();
            result.raw_bytes = raw_bytes;
            result.sanitized_bytes = sanitized_bytes;
            result.truncated = truncated;
            result.excerpt = Some(excerpt);
            result.outcome = CiLogOutcome::Available;
            result.captured_at = Utc::now();
            result
        })
        .collect()
}
