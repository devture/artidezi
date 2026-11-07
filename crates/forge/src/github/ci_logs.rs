// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Automatically selected, bounded GitHub Actions job evidence. This never follows check-run URLs
//! or fetches complete workflow archives. Missing optional evidence does not change the CI gate.

use std::collections::VecDeque;

use chrono::Utc;
use serde::Deserialize;

use super::GithubForge;
use crate::{CiJobLog, CiLogOutcome, ForgeError};
use artidezi_contract::common::CommitSha;

const MAX_RUNS: usize = 20;
const MAX_JOBS_PER_RUN: usize = 100;
const MAX_EXCERPT: usize = 3072;
const ERROR_SEARCH_WINDOW: usize = 64 * 1024;
const MAX_SELECTED_JOBS: usize = 2;

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

fn unavailable(name: &str, outcome: CiLogOutcome) -> CiJobLog {
    CiJobLog {
        name: name.into(),
        job_id: None,
        run_id: None,
        run_attempt: None,
        outcome,
        captured_at: Utc::now(),
        raw_bytes: 0,
        sanitized_bytes: 0,
        truncated: false,
        excerpt: None,
    }
}

fn failure(error: &ForgeError) -> CiLogOutcome {
    match error {
        ForgeError::AuthenticationRejected => CiLogOutcome::PermissionDenied,
        ForgeError::NotFound { .. } => CiLogOutcome::MissingOrInaccessible,
        ForgeError::Other(message) if message.contains("permission") => {
            CiLogOutcome::PermissionDenied
        }
        _ => CiLogOutcome::FetchFailed,
    }
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

#[derive(Default)]
struct LogCollector {
    pending_utf8: Vec<u8>,
    state: EscapeState,
    last_was_cr: bool,
    head: String,
    head_full: bool,
    tail: VecDeque<char>,
    tail_bytes: usize,
    sanitized_bytes: u64,
}

#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

impl LogCollector {
    fn push(&mut self, chunk: &[u8]) {
        let mut data = std::mem::take(&mut self.pending_utf8);
        data.extend_from_slice(chunk);
        let mut remaining = data.as_slice();
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(text) => {
                    for ch in text.chars() {
                        self.char(ch);
                    }
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    let prefix =
                        std::str::from_utf8(&remaining[..valid]).expect("validated prefix");
                    for ch in prefix.chars() {
                        self.char(ch);
                    }
                    match error.error_len() {
                        Some(invalid) => {
                            self.char('\u{fffd}');
                            remaining = &remaining[valid + invalid..];
                        }
                        None => {
                            self.pending_utf8.extend_from_slice(&remaining[valid..]);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn char(&mut self, ch: char) {
        self.state = match self.state {
            EscapeState::Text if ch == '\u{1b}' => EscapeState::Escape,
            EscapeState::Text => {
                if ch == '\r' {
                    self.emit('\n');
                    self.last_was_cr = true;
                } else if ch == '\n' && self.last_was_cr {
                    self.last_was_cr = false;
                } else {
                    self.last_was_cr = false;
                    if ch == '\n' || ch == '\t' || !ch.is_control() {
                        self.emit(ch);
                    }
                }
                EscapeState::Text
            }
            EscapeState::Escape if ch == '[' => EscapeState::Csi,
            EscapeState::Escape if ch == ']' => EscapeState::Osc,
            EscapeState::Escape => EscapeState::Text,
            EscapeState::Csi if ('@'..='~').contains(&ch) => EscapeState::Text,
            EscapeState::Csi => EscapeState::Csi,
            EscapeState::Osc if ch == '\u{7}' => EscapeState::Text,
            EscapeState::Osc if ch == '\u{1b}' => EscapeState::OscEscape,
            EscapeState::Osc => EscapeState::Osc,
            EscapeState::OscEscape if ch == '\\' => EscapeState::Text,
            EscapeState::OscEscape => EscapeState::Osc,
        };
    }

    fn emit(&mut self, ch: char) {
        let bytes = ch.len_utf8();
        self.sanitized_bytes += bytes as u64;
        if !self.head_full {
            if self.head.len() + bytes <= 512 {
                self.head.push(ch);
            } else {
                self.head_full = true;
            }
        }
        self.tail.push_back(ch);
        self.tail_bytes += bytes;
        while self.tail_bytes > ERROR_SEARCH_WINDOW {
            self.tail_bytes -= self.tail.pop_front().expect("nonempty tail").len_utf8();
        }
    }

    fn finish(mut self) -> (String, u64, bool) {
        if !self.pending_utf8.is_empty() {
            self.char('\u{fffd}');
        }
        if self.sanitized_bytes <= MAX_EXCERPT as u64 {
            return (self.tail.into_iter().collect(), self.sanitized_bytes, false);
        }
        if let Some(end) = self.head.rfind('\n').filter(|end| *end >= 256) {
            self.head.truncate(end + 1);
        }
        let searchable: String = self.tail.into_iter().collect();
        // Actions cleanup and Molecule teardown can bury the actual failure far before the
        // final log lines. Python exceptions end their tracebacks; the opening frames alone
        // often say nothing useful. Keep the preceding exception too when a wrapper fails.
        let diagnostics = diagnostic_ranges(&searchable);
        let diagnostic_bytes: usize = diagnostics.iter().map(|range| range.len()).sum();
        // At most four sections (head, two diagnostics, tail), with bounded omission markers.
        let tail_limit = MAX_EXCERPT - self.head.len() - diagnostic_bytes - 4 * 80;
        let tail_start =
            char_boundary_after(&searchable, searchable.len().saturating_sub(tail_limit));
        let offset = self.sanitized_bytes - searchable.len() as u64;
        let mut sections = vec![(0, self.head.as_str())];
        sections.extend(
            diagnostics
                .into_iter()
                .map(|range| (offset + range.start as u64, &searchable[range])),
        );
        sections.push((offset + tail_start as u64, &searchable[tail_start..]));
        sections.sort_by_key(|(start, _)| *start);
        let mut output = String::new();
        let mut cursor = 0;
        for (start, section) in sections {
            let end = start + section.len() as u64;
            if end <= cursor {
                continue;
            }
            if start > cursor {
                output.push_str(&format!(
                    "\n[... {} sanitized bytes omitted ...]\n",
                    start - cursor
                ));
            }
            output.push_str(&section[cursor.saturating_sub(start) as usize..]);
            cursor = end;
        }
        debug_assert!(output.len() <= MAX_EXCERPT);
        (output, self.sanitized_bytes, true)
    }
}

fn char_boundary_after(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn diagnostic_ranges(log: &str) -> Vec<std::ops::Range<usize>> {
    let error = ["fatal: [", "[ERROR]:"]
        .into_iter()
        .find_map(|marker| log.rfind(marker));
    if let Some(error) = error {
        let start = log[..error].rfind('\n').map_or(0, |at| at + 1);
        let mut end = (start + 1400).min(log.len());
        while !log.is_char_boundary(end) {
            end -= 1;
        }
        return std::iter::once(start..end).collect();
    }
    log.rmatch_indices("Traceback (most recent call last)")
        .take(2)
        .map(|(header, _)| {
            let start = log[..header].rfind('\n').map_or(0, |at| at + 1);
            let mut end = start;
            for (index, line) in log[start..].split_inclusive('\n').enumerate() {
                end += line.len();
                let text = log_line_text(line).trim_end();
                if index > 0 && !text.is_empty() && !text.starts_with(char::is_whitespace) {
                    // The first unindented line after the frames is the exception message.
                    break;
                }
            }
            char_boundary_after(log, end.saturating_sub(700).max(start))..end
        })
        .collect()
}

fn log_line_text(line: &str) -> &str {
    line.split_once(' ')
        .filter(|(prefix, _)| chrono::DateTime::parse_from_rfc3339(prefix).is_ok())
        .map_or(line, |(_, text)| text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_truncation_are_explicit_and_bounded() {
        let mut collector = LogCollector::default();
        collector.push(b"a\x1b[31mred\x1b[0m\r");
        collector.push(b"\nb\x1b]0;secret\x07c\0");
        assert_eq!(collector.finish().0, "ared\nbc");
        let mut collector = LogCollector::default();
        for chunk in "x".repeat(9000).as_bytes().chunks(71) {
            collector.push(chunk);
        }
        let (short, bytes, truncated) = collector.finish();
        assert_eq!(bytes, 9000);
        assert!(truncated);
        assert!(short.len() <= MAX_EXCERPT);
        assert!(short.contains("sanitized bytes omitted"));
        let mut collector = LogCollector::default();
        collector.push(&[b'a', 0xf0, 0x9f]);
        collector.push(&[0x98, 0x80, b'b']);
        assert_eq!(collector.finish().0, "a😀b");
    }

    #[test]
    fn diagnostic_before_long_cleanup_survives_truncation() {
        let mut collector = LogCollector::default();
        collector.push(b"job start\n");
        collector.push("setup\n".repeat(9000).as_bytes());
        collector.push(b"[ERROR]: Task failed: image pull failed\n");
        collector
            .push(b"fatal: [host]: FAILED! => manifest unknown for ghcr.io/example/image:v2\n");
        collector.push("cleanup\n".repeat(1500).as_bytes());
        let (excerpt, bytes, truncated) = collector.finish();
        assert!(truncated);
        assert!(bytes > ERROR_SEARCH_WINDOW as u64);
        assert!(excerpt.len() <= MAX_EXCERPT);
        assert!(excerpt.contains("manifest unknown for ghcr.io/example/image:v2"));
        assert!(excerpt.contains("cleanup"));
    }

    #[test]
    fn long_chained_python_tracebacks_preserve_both_exceptions() {
        for timestamp in ["", "2026-10-21T11:10:36.8767633Z "] {
            let mut log = "setup\n".repeat(9000);
            for exception in [
                "AttributeError: module 'dataclasses' has no attribute '_is_type'",
                "subprocess.CalledProcessError: Command '['ansible-config', 'dump']' returned non-zero exit status 1.",
            ] {
                log.push_str(&format!("{timestamp}Traceback (most recent call last):\n"));
                for _ in 0..60 {
                    log.push_str(&format!("{timestamp}  File \"/example/runtime.py\", line 45, in invoke\n{timestamp}    invoke_callback()\n"));
                }
                log.push_str(&format!("{timestamp}{exception}\n"));
            }
            log.push_str(&"cleanup\n".repeat(1500));
            let mut collector = LogCollector::default();
            for chunk in log.as_bytes().chunks(71) {
                collector.push(chunk);
            }
            let (excerpt, bytes, truncated) = collector.finish();
            assert_eq!(bytes, log.len() as u64);
            assert!(truncated);
            assert!(excerpt.len() <= MAX_EXCERPT);
            assert!(
                excerpt
                    .contains("AttributeError: module 'dataclasses' has no attribute '_is_type'")
            );
            assert!(excerpt.contains("subprocess.CalledProcessError:"));
            assert!(excerpt.ends_with("cleanup\n"));
        }
    }

    #[test]
    fn overlapping_python_sections_preserve_utf8_and_omission_bounds() {
        let log = format!(
            "{}Traceback (most recent call last):\n{}ValueError: invalid café\n{}",
            "é".repeat(150),
            "  frame 🦀\n".repeat(10),
            "cleanup 🦀\n".repeat(300),
        );
        let mut collector = LogCollector::default();
        collector.push(log.as_bytes());
        let (excerpt, bytes, truncated) = collector.finish();
        assert_eq!(bytes, log.len() as u64);
        assert!(truncated);
        assert!(excerpt.len() <= MAX_EXCERPT);
        assert!(excerpt.contains("ValueError: invalid café"));
        assert_eq!(excerpt.matches("ValueError:").count(), 1);
        assert!(excerpt.ends_with("cleanup 🦀\n"));
    }
}
