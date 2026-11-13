// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Fixed transient-error categories from terminal harness events, never repository/tool output.

/// Only terminal Codex errors qualify. Embedded command output and agent prose are ignored.
pub fn codex_failure(stream: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(stream);
    let event = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|event| event["type"] == "turn.failed")?;
    let message = event
        .pointer("/error/message")?
        .as_str()?
        .to_ascii_lowercase();
    if message.contains("selected model is at capacity") || message.contains("model is overloaded")
    {
        Some("model_capacity")
    } else if !message.contains("insufficient_quota")
        && !message.contains("usage limit")
        && (message.contains("rate limit exceeded")
            || message.contains("too many requests")
            || message.contains("429 too many"))
    {
        Some("model_rate_limit")
    } else if [
        "503 service unavailable",
        "502 bad gateway",
        "504 gateway timeout",
        "temporarily unavailable",
        "stream disconnected before completion",
        "connection reset",
        // Policy: a terminal timeout, such as "workspace routing discovery timed out" after Codex's
        // reconnection attempts, is worth a bounded retry. Retries start fresh from frozen inputs.
        "timed out",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
    {
        Some("model_transport")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_terminal_transient_errors_qualify() {
        for (text, expected) in [
            (
                r#"{"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}"#,
                Some("model_capacity"),
            ),
            (
                r#"{"type":"turn.failed","error":{"message":"429 Too Many Requests"}}"#,
                Some("model_rate_limit"),
            ),
            (
                r#"{"type":"turn.failed","error":{"message":"503 Service Unavailable"}}"#,
                Some("model_transport"),
            ),
            (
                r#"{"type":"turn.failed","error":{"message":"workspace routing discovery timed out"}}"#,
                Some("model_transport"),
            ),
            (
                r#"{"type":"item.completed","item":{"aggregated_output":"Selected model is at capacity"}}"#,
                None,
            ),
            (
                r#"{"type":"item.completed","item":{"aggregated_output":"test timed out"}}"#,
                None,
            ),
            (
                r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#,
                None,
            ),
            (
                r#"{"type":"turn.failed","error":{"message":"429 insufficient_quota"}}"#,
                None,
            ),
        ] {
            assert_eq!(codex_failure(text.as_bytes()), expected);
        }
    }
}
