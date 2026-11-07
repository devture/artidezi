// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use artidezi_contract::attempt::{AttemptStatus, VerificationStep};
use artidezi_store::ArtifactStore;
use std::collections::BTreeSet;

const MAX_DIAGNOSTIC_LOG_BYTES: u64 = 64 * 1024;

/// Only fixed categories and recorded exit facts reach the page. Harness logs may contain
/// credentials or untrusted repository text, so never interpolate any of their bytes into HTML.
pub(in crate::ui) fn missing_result_diagnostic(
    record: &AttemptRecord,
    store: &ArtifactStore,
) -> String {
    let mut summary = match record.status {
        AttemptStatus::Timeout => {
            "The worker reported that the harness timed out before writing a result.".to_owned()
        }
        AttemptStatus::CollectionFailed => {
            "The worker could not collect the workspace after the harness ran.".to_owned()
        }
        AttemptStatus::HarnessError if record.harness.exit_code.is_none() => {
            "The worker reported a harness launch or crash error before a result was written."
                .to_owned()
        }
        _ => match record.harness.exit_code {
            Some(0) => "The harness exited successfully but wrote no result document.".to_owned(),
            Some(code) => {
                format!("The worker reported harness exit code {code} before a result was written.")
            }
            None => "The worker recorded no harness exit code or result document.".to_owned(),
        },
    };

    let mut rejected_refresh = false;
    if record.harness.name == "codex"
        && let Some(reference) = &record.harness.stderr
        && reference.size <= MAX_DIAGNOSTIC_LOG_BYTES
        && record.artifacts.iter().any(|entry| {
            entry.name == "harness-stderr.log"
                && entry.sha256 == reference.sha256
                && entry.size == reference.size
        })
        && let Ok(stderr) = store.get(reference)
    {
        let stderr = String::from_utf8_lossy(&stderr).to_ascii_lowercase();
        rejected_refresh = stderr.contains("refresh_token_reused")
            || (stderr.contains("refresh token")
                && (stderr.contains("already been used")
                    || stderr.contains("invalid")
                    || stderr.contains("expired")
                    || stderr.contains("rejected")));
        if rejected_refresh {
            summary.push_str(" Captured Codex diagnostics indicate a rejected refresh token; the worker login may need renewal.");
        }
    }
    if record.harness.name == "codex"
        && !rejected_refresh
        && let Some(reference) = &record.harness.stdout
        && reference.size <= MAX_DIAGNOSTIC_LOG_BYTES
        && record.artifacts.iter().any(|entry| {
            entry.name == "harness-stdout.log"
                && entry.sha256 == reference.sha256
                && entry.size == reference.size
        })
        && let Ok(stdout) = store.get(reference)
        && std::str::from_utf8(&stdout).is_ok_and(|stream| {
            stream.lines().any(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|event| {
                    event.get("type").and_then(|value| value.as_str()) == Some("turn.failed")
                        && event
                            .pointer("/error/message")
                            .and_then(|value| value.as_str())
                            .is_some_and(|message| {
                                message.contains("401 Unauthorized")
                                    && message.contains("Incorrect API key provided")
                            })
                })
            })
        })
    {
        summary.push_str(" Captured Codex diagnostics indicate an HTTP 401 authentication rejection; check whether the worker login recovered before retrying.");
    }
    if record.harness.name == "codex"
        && let Some(reference) = &record.harness.stdout
        && record.artifacts.iter().any(|entry| {
            entry.name == "harness-stdout.log"
                && entry.sha256 == reference.sha256
                && entry.size == reference.size
        })
        && let Ok(tail) = store.tail(reference, MAX_DIAGNOSTIC_LOG_BYTES)
        && let Some(reason) = artidezi_contract::transient::codex_failure(&tail)
    {
        summary.push_str(match reason {
            "model_capacity" => {
                " The model provider reported that the selected model was at capacity."
            }
            "model_rate_limit" => " The model provider reported a temporary rate limit.",
            _ => " The model provider reported a temporary connection or availability failure.",
        });
    }
    summary
}

fn artifact<'a>(
    items: &'a [logs::LogView],
    reference: &ArtifactRef,
    name: &str,
) -> Option<&'a logs::LogView> {
    // Identical empty streams share a hash. Prefer the recorded stream name when available.
    let matches = |log: &&logs::LogView| {
        log.entry.sha256 == reference.sha256 && log.entry.size == reference.size
    };
    items
        .iter()
        .filter(matches)
        .find(|log| log.entry.name == name)
        .or_else(|| items.iter().find(matches))
}

fn link(task: &Task, view: &AttemptView, log: &logs::LogView, label: &str, kind: &str) -> String {
    let count = log
        .lines
        .map(|n| format!("{n} {}", if n == 1 { "line" } else { "lines" }))
        .unwrap_or_else(|| format!("{} KB", log.entry.size.div_ceil(1024)));
    let (route, target, description) = match kind {
        "patch" => ("patches", "", "View the patch"),
        "stdout" => (
            "logs",
            r#"target="_blank" rel="noopener""#,
            "Standard output",
        ),
        "stderr" => (
            "logs",
            r#"target="_blank" rel="noopener""#,
            "Standard error and diagnostics",
        ),
        _ => (
            "logs",
            r#"target="_blank" rel="noopener""#,
            "Stored evidence",
        ),
    };
    format!(
        r#"<a class="log-link {kind} {}" href="/tasks/{}/attempts/{}/{route}/{}" title="{description}. Line counts describe the stored artifact." {target}>{}<span>{}</span> <span class="log-count">{count}</span></a>"#,
        if log.lines == Some(0) {
            "empty"
        } else if log.lines.is_none() {
            "uncounted"
        } else {
            ""
        },
        task.number,
        e(&view.attempt.id),
        e(&log.entry.sha256),
        icon(kind),
        e(label)
    )
}

fn stream(
    task: &Task,
    view: &AttemptView,
    reference: Option<&ArtifactRef>,
    name: &str,
    label: &str,
    kind: &str,
    used: &mut BTreeSet<String>,
) -> (String, bool) {
    let Some(reference) = reference else {
        return (
            format!(
                r#"<span class="missing-log">{} · Not captured</span>"#,
                e(label)
            ),
            false,
        );
    };
    let Some(log) = artifact(&view.logs, reference, name) else {
        return (
            format!(
                r#"<span class="missing-log">{} · Unavailable</span>"#,
                e(label)
            ),
            false,
        );
    };
    used.insert(log.entry.name.clone());
    (link(task, view, log, label, kind), log.lines == Some(0))
}

fn command(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./:-=@%+,".contains(&b))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn check_state(check: &VerificationStep) -> (&'static str, &'static str, &'static str) {
    if check.skipped {
        ("skipped", "Skipped", "minus")
    } else if check.timed_out {
        ("failed", "Timed out", "alert")
    } else if check.exit_code == Some(0) {
        ("passed", "Passed", "check")
    } else {
        ("failed", "Failed", "alert")
    }
}

pub(super) fn card(task: &Task, view: &AttemptView, timezone: &str) -> String {
    let a = &view.attempt;
    let state = match a.state {
        AttemptState::Queued => "Queued",
        AttemptState::Leased => "Running",
        AttemptState::Uploaded => "Validating",
        AttemptState::Valid => "Finished",
        AttemptState::Invalid => "Rejected",
        AttemptState::Lost => "Worker lost",
        AttemptState::Cancelled => "Cancelled",
    };
    let mut out = format!(
        r#"<article class="panel attempt" id="attempt-{}"><header class="card-header attempt-heading"><h2>Attempt {}</h2><span class="attempt-state {}">{state}</span></header><div class="attempt-metadata"><span class="muted">{}</span>"#,
        e(&a.id),
        a.number,
        a.state.as_str(),
        time(&a.created_at, timezone)
    );
    if let Some(record) = &view.record {
        out.push_str(&execution_facts(record));
    }
    out.push_str("</div>");
    if view.expired_input_bundles > 0 {
        out.push_str(r#"<p class="muted">Frozen repository inputs expired under the instance retention policy. Results, patches, logs and worker-produced changes remain available. Re-evaluation fetches fresh repository inputs.</p>"#);
    }
    if let Some(feedback) = &a.human_feedback {
        write!(out, r#"<section class="feedback"><h3>Feedback for this attempt</h3><p class="prewrap">{}</p></section>"#, e(feedback)).unwrap();
    }
    if let Some(result) = &view.result {
        write!(out, r#"<section class="attempt-section attempt-summary"><h3>Summary</h3><div class="markdown">{}</div>"#, markdown::render(&result.summary)).unwrap();
        for question in &result.questions_for_human {
            write!(out, r#"<p class="question">{}</p>"#, e(question)).unwrap();
        }
        out.push_str("</section>");
    } else if a.state == AttemptState::Queued {
        if let Some(at) = a.not_before.filter(|at| *at > chrono::Utc::now()) {
            write!(out, r#"<p class="retry-scheduled">Automatic retry scheduled for {}. No action is needed.</p>"#, time(&at, timezone)).unwrap();
        } else if let Some(reason) = &view.unschedulable {
            write!(
                out,
                r#"<p class="muted"><strong>Unschedulable.</strong> {}</p>"#,
                e(reason)
            )
            .unwrap();
        } else {
            out.push_str(r#"<p class="muted">Waiting for a compatible worker.</p>"#);
        }
    } else if a.state == AttemptState::Leased {
        out.push_str(
            r#"<p class="muted">The worker is preparing, running or checking this attempt.</p>"#,
        );
    }

    let mut used = BTreeSet::new();
    let mut located_problems = BTreeSet::new();
    if let Some(record) = &view.record
        && record
            .repositories
            .iter()
            .any(|r| !r.setup.is_empty() || !r.verification.is_empty())
    {
        out.push_str(r#"<section class="attempt-section attempt-checks"><h3>Checks</h3><p class="section-note">Defined by the project · Run by the worker</p>"#);
        for repo in &record.repositories {
            for (phase, label, steps) in [
                ("setup", "Setup", &repo.setup),
                ("verify", "Verification", &repo.verification),
            ] {
                for (index, check) in steps.iter().enumerate() {
                    let (class, status, symbol) = check_state(check);
                    write!(out, r#"<section class="check-card {class}"><div class="check-row {class}"><h4>{}</h4><span class="check-status">{}{status}</span></div><div class="check-meta"><span title="Repository">{}{}</span><span class="badge" title="{}">{label}</span>"#,
                            e(&check.name), icon(symbol), icon("project"), e(repo.id.as_str()),
                            if phase == "setup" { "Run before the agent starts" } else { "Run after the agent finishes" }).unwrap();
                    if let Some(code) = check.exit_code {
                        write!(out, "<span>Exit code {code}</span>").unwrap();
                    }
                    if let (Some(start), Some(end)) = (check.started_at, check.finished_at) {
                        let millis = (end - start).num_milliseconds().max(0);
                        write!(
                            out,
                            r#"<span title="Command runtime">{}</span>"#,
                            if millis < 1000 {
                                "&lt;1s".into()
                            } else {
                                format!("{}s", millis / 1000)
                            }
                        )
                        .unwrap();
                    }
                    out.push_str("</div>");
                    write!(out, r#"<div class="check-command"><span class="muted">Command</span><code>{}</code></div>"#, e(&command(&check.command))).unwrap();
                    if check.cwd != "." {
                        write!(
                            out,
                            r#"<p class="section-note">Directory <code>{}</code></p>"#,
                            e(&check.cwd)
                        )
                        .unwrap();
                    }
                    if check.skipped {
                        out.push_str(r#"<p class="section-note">Not run because an earlier step failed.</p>"#);
                    } else {
                        let (stdout, stdout_empty) = stream(
                            task,
                            view,
                            check.stdout.as_ref(),
                            &format!("{phase}-{}-{index}-stdout.log", repo.id),
                            "stdout",
                            "stdout",
                            &mut used,
                        );
                        let (stderr, stderr_empty) = stream(
                            task,
                            view,
                            check.stderr.as_ref(),
                            &format!("{phase}-{}-{index}-stderr.log", repo.id),
                            "stderr",
                            "stderr",
                            &mut used,
                        );
                        write!(out, r#"<div class="log-links check-logs" aria-label="Check output">{stdout}{stderr}</div>"#).unwrap();
                        if check.truncated {
                            out.push_str(
                                r#"<p class="section-note">Captured output was truncated.</p>"#,
                            );
                        } else if stdout_empty && stderr_empty {
                            out.push_str(r#"<p class="section-note empty-output">This command produced no output.</p>"#);
                        }
                    }
                    let prefix = format!(
                        "repository {}: {} step {} failed:",
                        repo.id,
                        if phase == "setup" {
                            "setup"
                        } else {
                            "verification"
                        },
                        check.name
                    );
                    for (i, problem) in a
                        .validation_problems
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| p.starts_with(&prefix))
                    {
                        write!(out, r#"<p class="check-verdict" title="{}">Blocked by orchestrator validation</p>"#, e(problem)).unwrap();
                        located_problems.insert(i);
                    }
                    out.push_str("</section>");
                }
            }
        }
        out.push_str("</section>");
    }
    let other_problems: Vec<_> = a
        .validation_problems
        .iter()
        .enumerate()
        .filter(|(i, _)| !located_problems.contains(i))
        .collect();
    if !other_problems.is_empty() {
        out.push_str(r#"<section class="attempt-section validation-findings"><h3>Orchestrator validation</h3>"#);
        for (_, problem) in other_problems {
            write!(out, r#"<p class="form-error">{}</p>"#, e(problem)).unwrap();
            if *problem == "no result document"
                && let Some(diagnostic) = &view.missing_result_diagnostic
            {
                write!(
                    out,
                    r#"<p class="failure-diagnostic">{}</p>"#,
                    e(diagnostic)
                )
                .unwrap();
            }
        }
        out.push_str("</section>");
    }
    let patches: Vec<_> = view.logs.iter().filter(|log| log.kind == "patch").collect();
    if !patches.is_empty() {
        out.push_str(r#"<section class="attempt-section attempt-patches"><h3>Patches</h3><div class="log-links">"#);
        for log in patches {
            out.push_str(&link(task, view, log, &log.label, "patch"));
            used.insert(log.entry.name.clone());
        }
        out.push_str("</div></section>");
    }
    if let Some(record) = &view.record {
        out.push_str(r#"<section class="attempt-section agent-logs"><h3>Agent logs</h3><div class="log-links">"#);
        for (name, reference) in [
            ("stdout", &record.harness.stdout),
            ("stderr", &record.harness.stderr),
        ] {
            out.push_str(
                &stream(
                    task,
                    view,
                    reference.as_ref(),
                    &format!("harness-{name}.log"),
                    name,
                    name,
                    &mut used,
                )
                .0,
            );
        }
        if let Some(reference) = &record.harness.transcript
            && let Some(log) = artifact(&view.logs, reference, "harness-transcript.jsonl")
        {
            out.push_str(&link(task, view, log, "Transcript", "file"));
            used.insert(log.entry.name.clone());
        }
        out.push_str("</div></section>");
    }
    let other: Vec<_> = view
        .logs
        .iter()
        .filter(|log| !used.contains(&log.entry.name))
        .collect();
    if !other.is_empty() {
        out.push_str(r#"<section class="attempt-section other-evidence"><h3>Other files</h3><div class="log-links">"#);
        for log in other {
            out.push_str(&link(task, view, log, &log.label, log.kind));
        }
        out.push_str("</div></section>");
    }
    out.push_str("</article>");
    out
}

/// Why no connected worker of the pool can run this job, if so.
pub(in crate::ui) fn unschedulable_reason(
    job: &artidezi_contract::Job,
    pool: &str,
    workers: &[Worker],
    lease_ttl_seconds: u64,
) -> Option<String> {
    let now = chrono::Utc::now();
    let connected: Vec<_> = workers
        .iter()
        .filter(|w| {
            w.worker_pool == pool && (now - w.last_seen).num_seconds() <= lease_ttl_seconds as i64
        })
        .collect();
    if connected.is_empty() {
        return Some(format!("No worker of pool {pool} is connected."));
    }
    let tagged: Vec<_> = connected
        .into_iter()
        .filter(|w| job.worker_requirements.met_by(&w.tags))
        .collect();
    if tagged.is_empty() {
        return Some(format!(
            "No connected worker has the required tags: {}.",
            job.worker_requirements.required_tags.join(", ")
        ));
    }
    if !tagged
        .iter()
        .any(|w| job.agent_profile.runs_on(&w.harnesses))
    {
        return Some(format!(
            "No suitable worker offers harness {:?} at version {}.",
            job.agent_profile.harness, job.agent_profile.harness_version_constraint
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use artidezi_contract::attempt::{HarnessRecord, SupervisorInfo};
    use artidezi_contract::common::SCHEMA_VERSION;

    #[test]
    fn identical_streams_keep_their_names_and_mismatched_records_stay_unavailable() {
        let reference = ArtifactRef {
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            size: 0,
        };
        let entries: Vec<_> = [
            "harness-stderr.log",
            "verify-library-0-stdout.log",
            "verify-application-0-stdout.log",
        ]
        .into_iter()
        .map(|name| logs::LogView {
            entry: ArtifactEntry {
                name: name.into(),
                sha256: reference.sha256.clone(),
                size: 0,
                media_type: "text/plain".into(),
            },
            label: name.into(),
            kind: "stdout",
            lines: Some(0),
        })
        .collect();
        assert_eq!(
            artifact(&entries, &reference, "verify-library-0-stdout.log")
                .unwrap()
                .entry
                .name,
            "verify-library-0-stdout.log"
        );
        assert_eq!(
            artifact(&entries, &reference, "verify-application-0-stdout.log")
                .unwrap()
                .entry
                .name,
            "verify-application-0-stdout.log"
        );
        // A legacy name can still resolve the exact content reference, but never another size/hash.
        assert!(artifact(&entries, &reference, "legacy.log").is_some());
        assert!(
            artifact(
                &entries,
                &ArtifactRef {
                    size: 1,
                    ..reference.clone()
                },
                "harness-stderr.log"
            )
            .is_none()
        );
        assert!(
            artifact(
                &entries,
                &ArtifactRef {
                    sha256: "0".repeat(64),
                    ..reference
                },
                "harness-stderr.log"
            )
            .is_none()
        );
    }

    #[test]
    fn check_outcomes_and_commands_remain_unambiguous() {
        let mut check: VerificationStep = serde_json::from_value(serde_json::json!({"name":"test", "command":["test", "-f", "file"], "cwd":".", "exit_code":1})).unwrap();
        assert_eq!(check_state(&check).1, "Failed");
        check.exit_code = Some(0);
        assert_eq!(check_state(&check).1, "Passed");
        check.timed_out = true;
        assert_eq!(check_state(&check).1, "Timed out");
        check.skipped = true;
        assert_eq!(check_state(&check).0, "skipped");
        assert_eq!(
            command(&["sh".into(), "-c".into(), "test -f 'file name'".into()]),
            "sh -c 'test -f '\\''file name'\\'''"
        );
        assert_eq!(command(&["echo".into(), "".into()]), "echo ''");
    }

    #[test]
    fn missing_result_uses_fixed_diagnostics_without_exposing_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let stderr = b"Your refresh token has already been used; secret=never-show-me";
        let reference = store.put(stderr).unwrap();
        let now = chrono::Utc::now();
        let mut record = AttemptRecord {
            schema_version: SCHEMA_VERSION,
            attempt_id: "attempt-test".into(),
            job: reference.clone(),
            status: AttemptStatus::Completed,
            supervisor: SupervisorInfo {
                worker_pool: "test".into(),
                worker_id: "worker".into(),
                version: "test".into(),
            },
            harness: HarnessRecord {
                name: "codex".into(),
                version: "test".into(),
                model: None,
                exit_code: Some(1),
                started_at: now,
                finished_at: now,
                session_id: None,
                usage: None,
                transcript: None,
                stdout: None,
                stderr: Some(reference.clone()),
            },
            result: None,
            repositories: vec![],
            artifacts: vec![ArtifactEntry {
                name: "harness-stderr.log".into(),
                sha256: reference.sha256.clone(),
                size: reference.size,
                media_type: "text/plain".into(),
            }],
        };
        let diagnostic = missing_result_diagnostic(&record, &store);
        assert!(diagnostic.contains("exit code 1"));
        assert!(diagnostic.contains("rejected refresh token"));
        assert!(!diagnostic.contains("never-show-me"));
        record.artifacts.clear();
        assert!(!missing_result_diagnostic(&record, &store).contains("refresh token"));
        let stdout = b"{\"type\":\"turn.failed\",\"error\":{\"message\":\"401 Unauthorized: Incorrect API key provided: never-show-me\"}}\n";
        let stdout_ref = store.put(stdout).unwrap();
        record.harness.stderr = None;
        record.harness.stdout = Some(stdout_ref.clone());
        record.artifacts.push(ArtifactEntry {
            name: "harness-stdout.log".into(),
            sha256: stdout_ref.sha256.clone(),
            size: stdout_ref.size,
            media_type: "application/x-ndjson".into(),
        });
        let diagnostic = missing_result_diagnostic(&record, &store);
        assert!(diagnostic.contains("HTTP 401 authentication rejection"));
        assert!(!diagnostic.contains("never-show-me"));
        record.artifacts.clear();
        assert!(!missing_result_diagnostic(&record, &store).contains("HTTP 401"));
        let mut long_log = vec![b'x'; 72 * 1024];
        long_log.extend_from_slice(b"\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"Selected model is at capacity. secret=never-show-me\"}}\n");
        let reference = store.put(&long_log).unwrap();
        record.harness.stdout = Some(reference.clone());
        record.artifacts.push(ArtifactEntry {
            name: "harness-stdout.log".into(),
            sha256: reference.sha256,
            size: reference.size,
            media_type: "application/x-ndjson".into(),
        });
        let diagnostic = missing_result_diagnostic(&record, &store);
        assert!(diagnostic.contains("selected model was at capacity"));
        assert!(!diagnostic.contains("never-show-me"));
        record.status = AttemptStatus::Timeout;
        assert!(missing_result_diagnostic(&record, &store).contains("timed out"));
        record.status = AttemptStatus::Completed;
        record.harness.exit_code = Some(0);
        assert!(missing_result_diagnostic(&record, &store).contains("exited successfully"));
    }
}
