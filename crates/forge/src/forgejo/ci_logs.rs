// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Bounded Forgejo Actions job evidence for failed commit statuses. Forgejo Actions reports each
//! job as a commit status whose target URL names the run and the job's position in it; that link,
//! not the job name, selects the job, because several workflows may share a job name. Statuses
//! from other CI systems are skipped. Missing optional evidence never changes the CI gate.

use chrono::Utc;
use serde::Deserialize;

use super::ForgejoForge;
use crate::ci_log::{LogCollector, MAX_SELECTED_JOBS, failure, unavailable};
use crate::{CiJobLog, CiLogOutcome};
use artidezi_contract::common::CommitSha;

/// Forgejo's largest page; more runs for one commit are refused rather than read partially.
const MAX_RUNS: usize = 50;
const MAX_JOBS_PER_RUN: usize = 100;

#[derive(Deserialize)]
struct RunPage {
    total_count: usize,
    workflow_runs: Vec<Run>,
}

#[derive(Deserialize)]
struct Run {
    id: u64,
    index_in_repo: u64,
    commit_sha: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    trigger_event: String,
    #[serde(default)]
    repository: Option<Repo>,
}

#[derive(Deserialize)]
struct Repo {
    full_name: String,
}

#[derive(Deserialize)]
struct Job {
    id: u64,
    run_id: u64,
    name: String,
    status: String,
    attempt: u64,
    #[serde(default)]
    html_url: String,
}

/// The run index and job position of an Actions link to `owner/repo`, such as
/// `/owner/repo/actions/runs/5/jobs/0` or the absolute job URL with a trailing `/attempt/1`.
/// Instances served below a subpath keep it as a prefix.
pub(super) fn actions_job_position(url: &str, owner: &str, repo: &str) -> Option<(u64, u64)> {
    let path = match url.split_once("://") {
        Some((_, rest)) => &rest[rest.find('/')?..],
        None => url,
    };
    let path = path.split(['?', '#']).next()?;
    let mut segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    if segments.len() >= 2 && segments[segments.len() - 2] == "attempt" {
        segments[segments.len() - 1].parse::<u64>().ok()?;
        segments.truncate(segments.len() - 2);
    }
    let [.., o, r, "actions", "runs", run, "jobs", job] = segments.as_slice() else {
        return None;
    };
    if !o.eq_ignore_ascii_case(owner) || !r.eq_ignore_ascii_case(repo) {
        return None;
    }
    Some((run.parse().ok()?, job.parse().ok()?))
}

pub(super) fn collect(
    forge: &ForgejoForge,
    remote: &str,
    head: &CommitSha,
    names: &[String],
) -> Vec<CiJobLog> {
    let Ok((owner, repo)) = ForgejoForge::split(remote) else {
        return Vec::new();
    };
    let statuses = match forge.statuses_for(owner, repo, head.as_str()) {
        Ok(statuses) => statuses,
        Err(error) => {
            return names
                .iter()
                .take(MAX_SELECTED_JOBS)
                .map(|name| unavailable(name, failure(&error)))
                .collect();
        }
    };
    let mut targets = Vec::new();
    for name in names {
        let Some(status) = statuses.iter().find(|s| s.context == *name) else {
            continue;
        };
        let Some(position) = actions_job_position(&status.target_url, owner, repo) else {
            continue;
        };
        targets.push((name.as_str(), status.status.as_str(), position));
    }
    targets.truncate(MAX_SELECTED_JOBS);
    if targets.is_empty() {
        return Vec::new();
    }
    let all = |outcome| {
        targets
            .iter()
            .map(|(name, _, _)| unavailable(name, outcome))
            .collect()
    };
    let page: RunPage = match forge.get_json(&format!(
        "/repos/{owner}/{repo}/actions/runs?head_sha={}&limit={MAX_RUNS}&page=1",
        head.as_str()
    )) {
        Ok(page) => page,
        Err(error) => return all(failure(&error)),
    };
    // A partial listing could pick a different run with a reused index.
    if page.total_count > MAX_RUNS
        || page.workflow_runs.len() != page.total_count
        || page.workflow_runs.iter().any(|run| {
            run.id == 0
                || !run.commit_sha.eq_ignore_ascii_case(head.as_str())
                || run
                    .repository
                    .as_ref()
                    .is_some_and(|r| !r.full_name.eq_ignore_ascii_case(remote))
        })
    {
        return all(CiLogOutcome::FetchFailed);
    }
    targets
        .into_iter()
        .map(|(name, state, (run_index, job_position))| {
            let mut result = unavailable(name, CiLogOutcome::MissingOrInaccessible);
            let Some(run) = page
                .workflow_runs
                .iter()
                .find(|run| run.index_in_repo == run_index)
            else {
                return result;
            };
            let jobs: Vec<Job> = match forge.get_json(&format!(
                "/repos/{owner}/{repo}/actions/runs/{}/jobs",
                run.id
            )) {
                Ok(jobs) => jobs,
                Err(error) => {
                    result.outcome = failure(&error);
                    return result;
                }
            };
            if jobs.len() > MAX_JOBS_PER_RUN || jobs.iter().any(|job| job.run_id != run.id) {
                result.outcome = CiLogOutcome::FetchFailed;
                return result;
            }
            let Some(job) = jobs.into_iter().find(|job| {
                actions_job_position(&job.html_url, owner, repo) == Some((run_index, job_position))
            }) else {
                return result;
            };
            // Forgejo names the status `<workflow> / <job> (<event>)`.
            let named = [&run.event, &run.trigger_event]
                .iter()
                .any(|event| name.ends_with(&format!(" / {} ({event})", job.name)));
            if job.id == 0 || job.attempt == 0 || !named {
                result.outcome = CiLogOutcome::FetchFailed;
                return result;
            }
            result.job_id = Some(job.id);
            result.run_id = Some(run.id);
            result.run_attempt = Some(job.attempt);
            match job.status.as_str() {
                "failure" if matches!(state, "failure" | "error") => {}
                "waiting" | "running" | "blocked" => {
                    result.outcome = CiLogOutcome::Pending;
                    return result;
                }
                _ => return result,
            }
            let path = format!(
                "/repos/{owner}/{repo}/actions/jobs/{}/logs?attempt={}",
                job.id, job.attempt
            );
            let mut collector = LogCollector::default();
            let raw_bytes = match forge.job_log(&path, |chunk| collector.push(chunk)) {
                Ok(bytes) => bytes,
                Err(outcome) => {
                    result.outcome = outcome;
                    return result;
                }
            };
            // A rerun that started while the log was read would make it the wrong attempt.
            let current: Job =
                match forge.get_json(&format!("/repos/{owner}/{repo}/actions/jobs/{}", job.id)) {
                    Ok(current) => current,
                    Err(error) => {
                        result.outcome = failure(&error);
                        return result;
                    }
                };
            if current.id != job.id
                || current.run_id != run.id
                || current.attempt != job.attempt
                || current.name != job.name
                || current.status != "failure"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    const HEAD: &str = "fe6c9544e8da08d862a01318d7475314a2fa8632";
    const LOG: &str = "2026-11-13T06:43:01.1913193Z \u{2b50} Run Main Build\n\
        2026-11-13T06:43:01.1995463Z compiling\n\
        2026-11-13T06:43:01.1996496Z error[E0308]: mismatched types\n\
        2026-11-13T06:43:01.2005829Z Job 'test' failed\n";

    /// Responses shaped like Forgejo 17 with a host-mode runner: a CI workflow whose `test` job
    /// failed, another workflow with a passing job also named `test`, and an external status.
    struct Fixture {
        job_status: &'static str,
        attempt_after_log: u64,
        total_runs: usize,
        log_requests: usize,
    }

    fn job(
        id: u64,
        run: u64,
        index: u64,
        position: u64,
        name: &str,
        status: &str,
        attempt: u64,
    ) -> Value {
        json!({"id": id, "run_id": run, "name": name, "status": status, "attempt": attempt,
            "html_url": format!("http://forgejo:3000/org/app/actions/runs/{index}/jobs/{position}/attempt/{attempt}")})
    }

    fn respond(fixture: &mut Fixture, path: &str) -> (u16, String) {
        let ci = |status: &str, attempt| job(41, 17, 5, 0, "test", status, attempt);
        let reply = match path {
            p if p.starts_with(&format!("/api/v1/repos/org/app/commits/{HEAD}/status?")) => json!({
            "sha": HEAD, "total_count": 4, "statuses": [
                {"context": "CI / test (pull_request)", "status": "failure", "target_url": "/org/app/actions/runs/5/jobs/0"},
                {"context": "CI / lint (pull_request)", "status": "success", "target_url": "/org/app/actions/runs/5/jobs/1"},
                {"context": "Other / test (pull_request)", "status": "success", "target_url": "/org/app/actions/runs/6/jobs/0"},
                {"context": "external/ci", "status": "failure", "target_url": "https://ci.example.org/builds/9"},
            ]}),
            p if p.starts_with("/api/v1/repos/org/app/actions/runs?") => {
                let runs = [(18, 6, "other.yml"), (17, 5, "ci.yml")].map(|(id, index, file)| json!({
                    "id": id, "index_in_repo": index, "commit_sha": HEAD, "event": "pull_request",
                    "trigger_event": "pull_request", "workflow_id": file, "repository": {"full_name": "org/app"}}));
                json!({"total_count": fixture.total_runs, "workflow_runs": runs})
            }
            "/api/v1/repos/org/app/actions/runs/17/jobs" => json!([
                ci(fixture.job_status, 1),
                job(42, 17, 5, 1, "lint", "success", 1),
            ]),
            "/api/v1/repos/org/app/actions/runs/18/jobs" => {
                json!([job(43, 18, 6, 0, "test", "success", 1)])
            }
            "/api/v1/repos/org/app/actions/jobs/41/logs?attempt=1" => {
                fixture.log_requests += 1;
                return (200, LOG.into());
            }
            "/api/v1/repos/org/app/actions/jobs/41" => {
                ci(fixture.job_status, fixture.attempt_after_log)
            }
            other => panic!("unexpected request {other}"),
        };
        (200, reply.to_string())
    }

    fn serve(fixture: Arc<Mutex<Fixture>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                let (status, body) = respond(&mut fixture.lock().unwrap(), &path);
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        url
    }

    fn collect_with(fixture: Fixture) -> (Vec<CiJobLog>, usize) {
        let fixture = Arc::new(Mutex::new(fixture));
        let forge = ForgejoForge::new(&serve(fixture.clone()), "token", "artidezi");
        let names = [
            "CI / test (pull_request)".to_owned(),
            "external/ci".to_owned(),
        ];
        let logs = collect(&forge, "org/app", &CommitSha(HEAD.into()), &names);
        let requests = fixture.lock().unwrap().log_requests;
        (logs, requests)
    }

    fn fixture() -> Fixture {
        Fixture {
            job_status: "failure",
            attempt_after_log: 1,
            total_runs: 2,
            log_requests: 0,
        }
    }

    #[test]
    fn a_failed_actions_status_yields_its_own_jobs_bounded_log() {
        let (logs, requests) = collect_with(fixture());
        assert_eq!(requests, 1);
        // The external status has no Actions job, so it has no log entry at all.
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        assert_eq!(log.name, "CI / test (pull_request)");
        assert_eq!(log.outcome, CiLogOutcome::Available);
        assert_eq!(
            (log.job_id, log.run_id, log.run_attempt),
            (Some(41), Some(17), Some(1))
        );
        assert_eq!(log.raw_bytes, LOG.len() as u64);
        assert!(!log.truncated);
        assert!(
            log.excerpt
                .as_ref()
                .unwrap()
                .contains("error[E0308]: mismatched types")
        );
    }

    #[test]
    fn reruns_running_jobs_and_partial_listings_give_no_log() {
        let (logs, requests) = collect_with(Fixture {
            attempt_after_log: 2,
            ..fixture()
        });
        assert_eq!((logs[0].outcome, requests), (CiLogOutcome::FetchFailed, 1));
        assert!(logs[0].excerpt.is_none());
        let (logs, requests) = collect_with(Fixture {
            job_status: "running",
            ..fixture()
        });
        assert_eq!((logs[0].outcome, requests), (CiLogOutcome::Pending, 0));
        let (logs, requests) = collect_with(Fixture {
            total_runs: 3,
            ..fixture()
        });
        assert_eq!((logs[0].outcome, requests), (CiLogOutcome::FetchFailed, 0));
    }

    #[test]
    fn only_actions_links_to_the_same_repository_name_a_job() {
        let at = |url| actions_job_position(url, "Org", "app");
        assert_eq!(at("/org/app/actions/runs/5/jobs/0"), Some((5, 0)));
        assert_eq!(
            at("https://code.example.org/sub/org/app/actions/runs/12/jobs/3/attempt/2"),
            Some((12, 3))
        );
        assert_eq!(at("/org/app/actions/runs/5/jobs/1?x=1"), Some((5, 1)));
        for other in [
            "",
            "https://ci.example.org/org/app/builds/5",
            "/org/other/actions/runs/5/jobs/0",
            "/other/app/actions/runs/5/jobs/0",
            "/org/app/actions/runs/x/jobs/0",
            "/org/app/actions/runs/5/jobs/0/logs",
            "/org/app/actions/runs/5",
        ] {
            assert_eq!(at(other), None, "{other}");
        }
    }
}
