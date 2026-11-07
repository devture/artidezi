// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Pi coding-agent adapter. Pi runs inside the worker VM boundary and emits JSON events.

use std::fs;
use std::path::PathBuf;

use artidezi_contract::job::{Harness, HarnessSandbox};

use crate::{
    HarnessAdapter, HarnessError, Invocation, InvocationContext, Result, TranscriptFacts,
    common_env,
};

pub struct PiHarness {
    pub binary: PathBuf,
}

impl HarnessAdapter for PiHarness {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn installed_version(&self) -> Result<String> {
        crate::version::installed_version(&self.binary)
    }

    fn build_invocation(&self, ctx: &InvocationContext<'_>) -> Result<Invocation> {
        if ctx.job.worker_requirements.harness_sandbox != HarnessSandbox::Disabled {
            return Err(HarnessError::Invalid("the Pi adapter currently implements only harness_sandbox: disabled (VM-boundary pools)".into()));
        }
        let endpoint = &ctx.job.agent_profile.model_endpoint;
        if endpoint.auth_mode != "openai_api_key" {
            return Err(HarnessError::Credential {
                credential_ref: endpoint.credential_ref.clone(),
                reason: format!(
                    "Pi currently supports only openai_api_key, not {}",
                    endpoint.auth_mode
                ),
            });
        }
        let credential_dir = ctx.credential_dir.ok_or_else(|| HarnessError::Credential {
            credential_ref: endpoint.credential_ref.clone(),
            reason: "no credential directory available on this worker".into(),
        })?;
        let key = fs::read_to_string(credential_dir.join("api_key")).map_err(|source| {
            HarnessError::Io {
                path: credential_dir.join("api_key"),
                source,
            }
        })?;
        let pi_dir = ctx.home.join(".pi/agent");
        fs::create_dir_all(&pi_dir).map_err(|source| HarnessError::Io {
            path: pi_dir.clone(),
            source,
        })?;
        let base_url = endpoint
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com/v1");
        let model = &ctx.job.agent_profile.model;
        let models = serde_json::json!({"providers":{"artidezi":{"baseUrl":base_url,"api":"openai-completions","apiKey":"$OPENAI_API_KEY","authHeader":true,"compat":{"supportsDeveloperRole":false,"supportsReasoningEffort":false},"models":[{"id":model,"reasoning":false}]}}});
        fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&models).unwrap(),
        )
        .map_err(|source| HarnessError::Io {
            path: pi_dir.join("models.json"),
            source,
        })?;

        let mut env = common_env(ctx);
        env.push(("PI_CODING_AGENT_DIR".into(), pi_dir.display().to_string()));
        env.push(("PI_OFFLINE".into(), "1".into()));
        env.push(("OPENAI_API_KEY".into(), key.trim().to_owned()));
        let mut args = vec![
            "--mode".into(),
            "json".into(),
            "--no-session".into(),
            "--provider".into(),
            "artidezi".into(),
            "--model".into(),
            model.clone(),
            "--no-approve".into(),
            "-p".into(),
        ];
        for (key, value) in &ctx.job.agent_profile.model_settings {
            if key == "thinking" {
                args.push("--thinking".into());
                args.push(value.as_str().unwrap_or("off").to_owned());
            }
        }
        let prompt = fs::read(ctx.task_document).map_err(|source| HarnessError::Io {
            path: ctx.task_document.to_path_buf(),
            source,
        })?;
        Ok(Invocation {
            program: self.binary.clone(),
            args,
            env,
            cwd: ctx.primary_repo_dir.to_path_buf(),
            stdin: Some(prompt),
            credential_updates: vec![],
        })
    }

    fn read_transcript(&self, stdout: &[u8]) -> TranscriptFacts {
        let mut facts = TranscriptFacts::default();
        for line in String::from_utf8_lossy(stdout).lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(id) = event
                .get("sessionId")
                .or_else(|| event.get("session_id"))
                .and_then(|v| v.as_str())
            {
                facts.session_id = Some(id.to_owned());
            }
            if let Some(model) = event.get("model").and_then(|v| v.as_str()) {
                facts.model = Some(model.to_owned());
            }
            // Pi repeats the same usage object on message_start, message_end,
            // turn_end and agent_end. Count completed assistant messages only;
            // each message_end represents one model request.
            if event.get("type").and_then(|v| v.as_str()) == Some("message_end") {
                let usage = event
                    .get("usage")
                    .or_else(|| event.pointer("/message/usage"));
                if let Some(n) = usage
                    .and_then(|v| v.get("input").or_else(|| v.get("input_tokens")))
                    .and_then(|v| v.as_u64())
                {
                    facts.input_tokens =
                        Some(facts.input_tokens.unwrap_or_default().saturating_add(n));
                }
                if let Some(n) = usage
                    .and_then(|v| v.get("output").or_else(|| v.get("output_tokens")))
                    .and_then(|v| v.as_u64())
                {
                    facts.output_tokens =
                        Some(facts.output_tokens.unwrap_or_default().saturating_add(n));
                }
            }
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_facts_accept_pi_json_events() {
        let harness = PiHarness {
            binary: PathBuf::from("pi"),
        };
        let facts = harness.read_transcript(
            br#"{"type":"session","sessionId":"sess-1"}
{"type":"message_end","model":"demo-model","usage":{"input":12,"output":7}}
{"type":"turn_end","message":{"usage":{"input":12,"output":7}}}
{"type":"message_end","model":"demo-model","usage":{"input":5,"output":3}}
"#,
        );
        assert_eq!(facts.session_id.as_deref(), Some("sess-1"));
        assert_eq!(facts.model.as_deref(), Some("demo-model"));
        assert_eq!(facts.input_tokens, Some(17));
        assert_eq!(facts.output_tokens, Some(10));
    }
}
