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

/// Whether the provider refused a Codex run over credentials or quota, which time alone does not fix.
pub fn codex_refused(stream: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stream);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|event| event["type"] == "turn.failed")
        .and_then(|event| {
            let message = event.pointer("/error/message")?.as_str()?;
            Some(message.to_ascii_lowercase())
        })
        .is_some_and(|message| {
            [
                "insufficient_quota",
                "usage limit",
                "quota exceeded",
                "401 unauthorized",
                "403 forbidden",
                "invalid_api_key",
                "token expired",
                "not logged in",
            ]
            .iter()
            .any(|pattern| message.contains(pattern))
        })
}

/// Claude Code's final `result` event, when it reports a failed run. Anything the session did after
/// an earlier result makes that one no longer final.
fn claude_error(stream: &[u8]) -> Option<serde_json::Value> {
    let text = String::from_utf8_lossy(stream);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|event| {
            matches!(
                event["type"].as_str(),
                Some("result" | "assistant" | "user")
            )
        })
        .filter(|event| event["type"] == "result" && event["is_error"] == true)
}

/// Claude Code's own description of an API error, lowercased. With `terminal_reason: api_error` it
/// comes from Claude Code and the provider, not from the model.
fn claude_message(event: &serde_json::Value) -> String {
    event["result"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        // "Server is temporarily limiting requests (not your usage limit)"
        .replace("not your usage limit", "")
}

/// Only Claude Code's final error result qualifies, by its structured fields. Budget and turn limits
/// end a run with another reason than `api_error`.
pub fn claude_failure(stream: &[u8]) -> Option<&'static str> {
    let event = claude_error(stream)?;
    if event["subtype"] != "success"
        || event["terminal_reason"] != "api_error"
        || claude_refusal(&event)
    {
        return None;
    }
    match (
        event["api_error_status"].as_u64(),
        event["api_error_type"].as_str(),
        event["api_error"].as_str(),
    ) {
        (Some(529), ..) | (_, Some("overloaded_error"), _) => Some("model_capacity"),
        (Some(429), None | Some("rate_limit_error"), None)
        | (None, Some("rate_limit_error"), None) => Some("model_rate_limit"),
        (Some(408 | 500..=599), ..)
        | (None, Some("api_error" | "timeout_error"), None)
        | (None, _, Some("connection_failed" | "no_response" | "stream_idle_timeout")) => {
            Some("model_transport")
        }
        // Claude Code gives these no code.
        (None, None, None)
            if ["", "api error: "].iter().any(|prefix| {
                [
                    "request timed out",
                    "stream idle timeout",
                    "connection error",
                ]
                .iter()
                .any(|text| claude_message(&event).starts_with(&format!("{prefix}{text}")))
            }) =>
        {
            Some("model_transport")
        }
        _ => None,
    }
}

/// Whether a Claude Code run ended over credentials, permissions, quota or its own budget or turn
/// limit, which time alone does not fix.
pub fn claude_refused(stream: &[u8]) -> bool {
    claude_error(stream).is_some_and(|event| claude_refusal(&event))
}

fn claude_refusal(event: &serde_json::Value) -> bool {
    if matches!(
        event["subtype"].as_str(),
        Some("error_max_budget_usd" | "error_max_turns")
    ) || matches!(
        event["terminal_reason"].as_str(),
        Some("budget_exhausted" | "max_turns")
    ) {
        return true;
    }
    if event["terminal_reason"] != "api_error" {
        return false;
    }
    // 404 is a model that does not exist or that the key may not use.
    matches!(event["api_error_status"].as_u64(), Some(401..=404))
        || matches!(
            event["api_error_type"].as_str(),
            Some("authentication_error" | "billing_error" | "not_found_error" | "permission_error")
        )
        || [
            "credential",
            "credits",
            "login",
            "quota",
            "api_key",
            "signin",
            "spend_limit",
            "usage_limit",
        ]
        .iter()
        .any(|text| {
            ["api_error", "api_error_code", "api_error_type"]
                .iter()
                .any(|field| {
                    event[field]
                        .as_str()
                        .is_some_and(|code| code.contains(text))
                })
        })
        || (matches!(event["api_error_status"].as_u64(), None | Some(400 | 429)) && {
            // The provider reports an exhausted quota as an invalid request or a rate limit.
            let message = claude_message(event);
            [
                "billing",
                "credit balance",
                "extra usage",
                "invalid api key",
                "not logged in",
                "quota",
                "spend limit",
                "spending limit",
                "usage credits",
                "usage limit",
            ]
            .iter()
            .any(|text| message.contains(text))
        })
}

/// The temporary provider failure that ended a run of the named harness.
pub fn failure(harness: &str, stream: &[u8]) -> Option<&'static str> {
    match harness {
        "claude" => claude_failure(stream),
        "codex" => codex_failure(stream),
        _ => None,
    }
}

/// Whether the named harness's run ended over something that another run of it does not fix.
pub fn refused(harness: &str, stream: &[u8]) -> bool {
    match harness {
        "claude" => claude_refused(stream),
        "codex" => codex_refused(stream),
        _ => false,
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
            assert_eq!(
                codex_refused(text.as_bytes()),
                text.contains("401") || text.contains("insufficient_quota")
            );
        }
        assert!(!codex_refused(
            br#"{"type":"item.completed","item":{"aggregated_output":"401 Unauthorized"}}"#
        ));
    }

    /// Results of Claude Code 2.1.330, reduced to the fields that matter.
    #[test]
    fn only_claudes_final_structured_error_qualifies() {
        let api = |fields: &str, message: &str| {
            format!(
                r#"{{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error",{fields},"result":{}}}"#,
                serde_json::Value::from(message)
            )
        };
        let status = |status: u16, kind: &str, message: &str| {
            api(
                &format!(r#""api_error_status":{status},"api_error_type":"{kind}""#),
                message,
            )
        };
        let code = |code: &str, message: &str| {
            api(
                &format!(r#""api_error_status":null,"api_error":"{code}""#),
                message,
            )
        };
        let none = r#""api_error_status":null"#;
        for (event, expected, refused) in [
            (status(529, "overloaded_error", "API Error: 529 Overloaded."), Some("model_capacity"), false),
            (api(r#""api_error_status":null,"api_error_type":"overloaded_error""#, "API Error: Overloaded"), Some("model_capacity"), false),
            (status(429, "rate_limit_error", "API Error: Request rejected (429) · This request would exceed your organization's rate limit of 80,000 input tokens per minute"), Some("model_rate_limit"), false),
            (status(429, "rate_limit_error", "API Error: Server is temporarily limiting requests (not your usage limit) · Try again"), Some("model_rate_limit"), false),
            (status(500, "api_error", "API Error: 500 Internal server error."), Some("model_transport"), false),
            (status(503, "api_error", "API Error: 503 Service Unavailable."), Some("model_transport"), false),
            (status(504, "timeout_error", "API Error: 504 Request timed out."), Some("model_transport"), false),
            (api(r#""api_error_status":null,"api_error_type":"api_error""#, "API Error: Internal server error"), Some("model_transport"), false),
            (status(503, "api_error", "API Error: 503 Billing service unavailable."), Some("model_transport"), false),
            (api(r#""api_error_status":502"#, "API Error: 502 Bad Gateway."), Some("model_transport"), false),
            (code("connection_failed", "API Error: Connection dropped (ECONNRESET)."), Some("model_transport"), false),
            (code("stream_idle_timeout", "API Error: No data arrived"), Some("model_transport"), false),
            (code("no_response", "API Error: No response from API"), Some("model_transport"), false),
            (api(none, "Request timed out. If the API is slow to answer, set API_TIMEOUT_MS above 3000."), Some("model_transport"), false),
            (api(none, "API Error: Connection error."), Some("model_transport"), false),
            // Never retried: credentials, permissions, quota and a run's own limits.
            (status(401, "authentication_error", "Invalid API key · Fix external API key"), None, true),
            (status(402, "billing_error", "API Error: 402 Billing"), None, true),
            (status(403, "permission_error", "Failed to authenticate. API Error: 403"), None, true),
            (status(404, "not_found_error", "There's an issue with the selected model"), None, true),
            (status(400, "invalid_request_error", "Credit balance is too low"), None, true),
            (status(400, "invalid_request_error", "API Error: 400 You have reached your specified API usage limits."), None, true),
            (status(429, "rate_limit_error", "API Error: Request rejected (429) · You have reached your specified API usage limits."), None, true),
            (status(429, "rate_limit_error", "API Error: Request rejected (429) · Extra usage is required for long context requests."), None, true),
            (api(r#""api_error_status":429,"api_error_code":"enforced_spend_limit_reached""#, "API Error: Request rejected (429)"), None, true),
            (api(r#""api_error_status":429,"api_error":"usage_limit_reached""#, "Limit reached"), None, true),
            (status(429, "insufficient_quota", "Limit reached"), None, true),
            (status(429, "some_error", "Limit reached"), None, false),
            (code("org_disabled_credential", "API Error"), None, true),
            (r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"terminal_reason":"budget_exhausted","errors":["Reached maximum budget ($3)"]}"#.into(), None, true),
            (r#"{"type":"result","subtype":"error_max_turns","is_error":true,"terminal_reason":"max_turns"}"#.into(), None, true),
            // Something else: neither known to pass nor a refusal.
            (status(400, "invalid_request_error", "API Error: 400 messages.1: something"), None, false),
            (status(413, "request_too_large", "Request too large (max 32MB)."), None, false),
            (code("max_output_tokens", "API Error: Claude's response exceeded the output token maximum."), None, false),
            (api(none, "API Error: Opus 5.5's safeguards flagged this message: request timed out"), None, false),
            (r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":529}"#.into(), None, false),
            (r#"{"type":"result","subtype":"success","is_error":true,"terminal_reason":"prompt_too_long","api_error_status":400,"result":"Prompt is too long"}"#.into(), None, false),
            // What the model or a command said, and what Claude Code retried by itself.
            (r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","api_error_status":null,"result":"API Error: 529 Overloaded, invalid api key"}"#.into(), None, false),
            (r#"{"type":"system","subtype":"api_retry","error_status":529,"error":"overloaded"}"#.into(), None, false),
            (r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"529 Overloaded"}]}}"#.into(), None, false),
            (r#"{"type":"user","message":{"content":"{\"type\":\"result\",\"is_error\":true,\"api_error_status\":529}"}}"#.into(), None, false),
        ] {
            let stream = format!("{{\"type\":\"assistant\"}}\n{event}\n{{\"type\":\"system\"}}\n");
            assert_eq!(claude_failure(stream.as_bytes()), expected, "{event}");
            assert_eq!(claude_refused(stream.as_bytes()), refused, "{event}");
            assert_eq!(codex_failure(stream.as_bytes()), None);
            assert!(!codex_refused(stream.as_bytes()));
            assert_eq!(failure("claude", stream.as_bytes()), expected);
            assert_eq!(failure("codex", stream.as_bytes()), None);
            assert_eq!(failure("pi", stream.as_bytes()), None);
        }
        // A result that the session went on after is not its final one.
        let error = status(529, "overloaded_error", "API Error: 529 Overloaded.");
        let refusal = status(401, "authentication_error", "Invalid API key");
        for more in [
            r#"{"type":"assistant"}"#,
            r#"{"type":"user"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed"}"#,
        ] {
            let stream = format!("{error}\n{refusal}\n{more}\n");
            assert_eq!(claude_failure(stream.as_bytes()), None);
            assert!(!claude_refused(stream.as_bytes()));
        }
    }
}
