// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral bounds and sanitization for CI job logs. Log text stays untrusted data; these
//! helpers only limit how much of it is kept and strip terminal control sequences.

use std::collections::VecDeque;

use chrono::Utc;

use crate::{CiJobLog, CiLogOutcome, ForgeError};

pub(crate) const MAX_EXCERPT: usize = 3072;
pub(crate) const ERROR_SEARCH_WINDOW: usize = 64 * 1024;
pub(crate) const MAX_SELECTED_JOBS: usize = 2;
pub(crate) const MAX_RAW_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn unavailable(name: &str, outcome: CiLogOutcome) -> CiJobLog {
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

pub(crate) fn failure(error: &ForgeError) -> CiLogOutcome {
    match error {
        ForgeError::AuthenticationRejected => CiLogOutcome::PermissionDenied,
        ForgeError::NotFound { .. } => CiLogOutcome::MissingOrInaccessible,
        ForgeError::Other(message) if message.contains("permission") => {
            CiLogOutcome::PermissionDenied
        }
        _ => CiLogOutcome::FetchFailed,
    }
}

/// Feeds a log body to `consume` in chunks, refusing more than `MAX_RAW_LOG_BYTES` on the wire or
/// after decoding. Returns the raw byte count.
pub(crate) fn read_bounded(
    response: &mut ureq::http::Response<ureq::Body>,
    mut consume: impl FnMut(&[u8]),
) -> Result<u64, CiLogOutcome> {
    if response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RAW_LOG_BYTES)
    {
        return Err(CiLogOutcome::TooLarge);
    }
    use std::io::Read;
    let mut reader = response.body_mut().as_reader();
    let mut raw_bytes = 0u64;
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .map_err(|_| CiLogOutcome::FetchFailed)?;
        if count == 0 {
            return Ok(raw_bytes);
        }
        raw_bytes += count as u64;
        if raw_bytes > MAX_RAW_LOG_BYTES {
            return Err(CiLogOutcome::TooLarge);
        }
        consume(&chunk[..count]);
    }
}

#[derive(Default)]
pub(crate) struct LogCollector {
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
    pub(crate) fn push(&mut self, chunk: &[u8]) {
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

    /// The bounded excerpt, the sanitized byte count and whether anything was omitted.
    pub(crate) fn finish(mut self) -> (String, u64, bool) {
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
