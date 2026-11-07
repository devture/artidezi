// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Codex adapter for a VM-boundary worker pool.
//!
//! Unattended Codex runs with `codex exec`, an ephemeral session, `danger-full-access` and no
//! approvals: the worker VM is the boundary, not the harness sandbox. Strict pools are not
//! implemented yet and are refused rather than approximated.
//!
//! The flag set below was confirmed against Codex 0.154.0 on the real worker (2026-09-11):
//! `codex exec` does not accept `--ask-for-approval` (exec never asks), the profile model
//! `default` means "omit `-m` and let Codex choose", reasoning effort is the Codex config key
//! `model_reasoning_effort`, and `CODEX_HOME` must not live under `/tmp` because Codex refuses
//! to create its helper binaries there. The `codex-code-mode-host` companion must be installed
//! next to `codex` or the agent has no execution tool at all.

use std::path::PathBuf;

use artidezi_contract::job::{Harness, HarnessSandbox, WebSearchMode};

use crate::{
    CredentialFileKind, CredentialFileUpdate, HarnessAdapter, HarnessError, Invocation,
    InvocationContext, Result, TranscriptFacts,
};

pub struct CodexHarness {
    pub binary: PathBuf,
}

/// A failed Codex turn may be started again only when its structured stream shows an
/// authentication rejection before any agent message or tool call. Unknown events fail closed.
/// Keep this narrow: replaying a turn that already worked in the repository could repeat effects.
pub fn prework_auth_rejection(stdout: &[u8]) -> bool {
    if stdout.len() > 64 * 1024 {
        return false;
    }
    let Ok(stream) = std::str::from_utf8(stdout) else {
        return false;
    };
    let mut rejected = false;
    for line in stream.lines() {
        if rejected {
            return false;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        match event.get("type").and_then(|value| value.as_str()) {
            Some("thread.started" | "turn.started" | "error") => {}
            Some("item.completed")
                if event.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("error")
                    && event
                        .pointer("/item/message")
                        .and_then(|value| value.as_str())
                        .is_some_and(|message| {
                            message.starts_with("Falling back from WebSockets to HTTPS transport.")
                        }) => {}
            Some("turn.failed") => {
                rejected = event
                    .pointer("/error/message")
                    .and_then(|value| value.as_str())
                    .is_some_and(|message| {
                        message.contains("401 Unauthorized")
                            && message.contains("Incorrect API key provided")
                    });
            }
            _ => return false,
        }
    }
    rejected
}

impl CodexHarness {
    fn copy_credentials(&self, ctx: &InvocationContext<'_>) -> Result<Vec<CredentialFileUpdate>> {
        let credential_ref = &ctx.job.agent_profile.model_endpoint.credential_ref;
        let auth_mode = ctx.job.agent_profile.model_endpoint.auth_mode.as_str();
        let Some(dir) = ctx.credential_dir else {
            return Err(HarnessError::Credential {
                credential_ref: credential_ref.clone(),
                reason: "no credential directory available on this worker".into(),
            });
        };
        let codex_home = ctx.home.join(".codex");
        std::fs::create_dir_all(&codex_home).map_err(|source| HarnessError::Io {
            path: codex_home.clone(),
            source,
        })?;
        match auth_mode {
            // An enrolled ChatGPT login lives in auth.json; it is copied into the fresh CODEX_HOME
            // so the attempt-local state is destroyed with the workspace afterwards.
            "chatgpt_login" => {
                let source = dir.join("auth.json");
                let target = codex_home.join("auth.json");
                std::fs::copy(&source, &target).map_err(|e| HarnessError::Credential {
                    credential_ref: credential_ref.clone(),
                    reason: format!("cannot copy {}: {e}", source.display()),
                })?;
                Ok(vec![CredentialFileUpdate {
                    credential_ref: credential_ref.clone(),
                    source,
                    attempt: target,
                    kind: CredentialFileKind::CodexChatgptAuth,
                }])
            }
            "openai_api_key" => Ok(vec![]),
            other => Err(HarnessError::Credential {
                credential_ref: credential_ref.clone(),
                reason: format!("auth mode {other} is not implemented by the Codex adapter"),
            }),
        }
    }
}

impl HarnessAdapter for CodexHarness {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn installed_version(&self) -> Result<String> {
        crate::version::installed_version(&self.binary)
    }

    fn build_invocation(&self, ctx: &InvocationContext<'_>) -> Result<Invocation> {
        if ctx.job.worker_requirements.harness_sandbox != HarnessSandbox::Disabled {
            return Err(HarnessError::Invalid(
                "the Codex adapter currently implements only harness_sandbox: disabled (VM-boundary pools)".into(),
            ));
        }
        let credential_updates = self.copy_credentials(ctx)?;

        let mut env = crate::common_env(ctx);
        env.push((
            "CODEX_HOME".into(),
            ctx.home.join(".codex").display().to_string(),
        ));
        if ctx.job.agent_profile.model_endpoint.auth_mode == "openai_api_key"
            && let Some(dir) = ctx.credential_dir
        {
            let key = std::fs::read_to_string(dir.join("api_key")).map_err(|e| {
                HarnessError::Credential {
                    credential_ref: ctx.job.agent_profile.model_endpoint.credential_ref.clone(),
                    reason: format!("cannot read api_key: {e}"),
                }
            })?;
            env.push(("OPENAI_API_KEY".into(), key.trim().to_owned()));
        }

        let web_search = match ctx.job.network.web_search.mode {
            WebSearchMode::Disabled => "disabled",
            WebSearchMode::Cached => "cached",
            WebSearchMode::Indexed => "indexed",
            WebSearchMode::Live => "live",
        };
        let mut args: Vec<String> = vec![
            "exec".into(),
            "--json".into(),
            "--ephemeral".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "danger-full-access".into(),
            "--cd".into(),
            ctx.primary_repo_dir.display().to_string(),
            "-c".into(),
            format!("web_search={web_search:?}"),
        ];
        if ctx.job.agent_profile.model != "default" {
            args.push("--model".into());
            args.push(ctx.job.agent_profile.model.clone());
        }
        for (key, value) in &ctx.job.agent_profile.model_settings {
            // Settings are opaque TOML-style overrides the profile author controls.
            args.push("-c".into());
            args.push(format!("{key}={}", toml_literal(value)));
        }
        if let Some(base_url) = &ctx.job.agent_profile.model_endpoint.base_url {
            args.push("-c".into());
            args.push(format!("model_providers.artidezi.base_url={base_url:?}"));
            args.push("-c".into());
            args.push("model_provider=\"artidezi\"".into());
        }
        args.push("-".into());

        let prompt = std::fs::read(ctx.task_document).map_err(|source| HarnessError::Io {
            path: ctx.task_document.to_path_buf(),
            source,
        })?;

        Ok(Invocation {
            program: self.binary.clone(),
            args,
            env,
            cwd: ctx.primary_repo_dir.to_path_buf(),
            stdin: Some(prompt),
            credential_updates,
        })
    }

    fn read_transcript(&self, stdout: &[u8]) -> TranscriptFacts {
        // `codex exec --json` emits one JSON event per line. Field names are taken from the
        // documented event stream; anything unexpected is ignored rather than failing the attempt.
        let mut facts = TranscriptFacts::default();
        for line in String::from_utf8_lossy(stdout).lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(id) = event
                .pointer("/thread_id")
                .or_else(|| event.pointer("/session_id"))
                .and_then(|v| v.as_str())
            {
                facts.session_id = Some(id.to_owned());
            }
            if let Some(model) = event.pointer("/model").and_then(|v| v.as_str()) {
                facts.model = Some(model.to_owned());
            }
            if let Some(usage) = event
                .pointer("/usage")
                .or_else(|| event.pointer("/info/total_token_usage"))
            {
                facts.input_tokens = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .or(facts.input_tokens);
                facts.output_tokens = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .or(facts.output_tokens);
            }
        }
        facts
    }
}

fn toml_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_facts_are_extracted_leniently() {
        let adapter = CodexHarness {
            binary: "codex".into(),
        };
        let stdout = b"{\"type\":\"thread.started\",\"thread_id\":\"thr_1\"}\nnot json\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}\n";
        let facts = adapter.read_transcript(stdout);
        assert_eq!(facts.session_id.as_deref(), Some("thr_1"));
        assert_eq!(facts.input_tokens, Some(12));
        assert_eq!(facts.output_tokens, Some(3));
    }

    #[test]
    fn auth_retry_requires_a_failed_turn_without_agent_work() {
        let stream = b"{\"type\":\"thread.started\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"error\",\"message\":\"Reconnecting...\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"error\",\"message\":\"Falling back from WebSockets to HTTPS transport.\"}}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"401 Unauthorized: Incorrect API key provided\"}}\n";
        assert!(prework_auth_rejection(stream));
        let mut after_failure = stream.to_vec();
        after_failure.extend_from_slice(
            b"{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\"}}\n",
        );
        assert!(!prework_auth_rejection(&after_failure));
        let mut worked =
            b"{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\"}}\n".to_vec();
        worked.extend_from_slice(stream);
        assert!(!prework_auth_rejection(&worked));
        assert!(!prework_auth_rejection(b"not json\n"));
        assert!(!prework_auth_rejection(
            b"{\"type\":\"turn.failed\",\"error\":{\"message\":\"refresh_token_reused\"}}\n"
        ));
    }
}
