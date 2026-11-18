// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Claude Code adapter for a VM-boundary worker pool.
//!
//! Claude Code runs headless (`claude -p`) with an Anthropic API key, permission checks bypassed
//! and a fixed tool set: the worker VM is the boundary, not the harness. Claude Code applies prompt
//! caching itself; the final `result` event reports cache reads/writes and its own cost estimate.
//!
//! The flag set below was confirmed against Claude Code 2.1.327 (2026-11-18): `--tools` limits
//! the built-in tools to the named ones, `--setting-sources ''` ignores user, project and local
//! settings files, `--effort` takes `low`, `medium`, `high`, `xhigh` or `max`, and a failed run
//! (rejected key, budget or turn limit) ends with an `is_error` result event and exit status 1.
//! Such a run is a harness failure even when the agent already wrote a result document.

use std::path::PathBuf;

use artidezi_contract::job::{Harness, HarnessSandbox, WebSearchMode, WebSearchScope};

use crate::{
    HarnessAdapter, HarnessError, Invocation, InvocationContext, Result, TranscriptFacts,
    common_env,
};

pub struct ClaudeHarness {
    pub binary: PathBuf,
}

/// Built-in tools an unattended attempt gets. Scheduling, worktree, workflow and messaging tools
/// are left out: they either outlive the process or multiply cost without helping a single task.
const TOOLS: &[&str] = &["Bash", "Read", "Edit", "Write", "Task"];
const WEB_TOOLS: &[&str] = &["WebSearch", "WebFetch"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

impl HarnessAdapter for ClaudeHarness {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn installed_version(&self) -> Result<String> {
        crate::version::installed_version(&self.binary)
    }

    fn build_invocation(&self, ctx: &InvocationContext<'_>) -> Result<Invocation> {
        if ctx.job.worker_requirements.harness_sandbox != HarnessSandbox::Disabled {
            return Err(HarnessError::Invalid(
                "the Claude adapter currently implements only harness_sandbox: disabled (VM-boundary pools)".into(),
            ));
        }
        let endpoint = &ctx.job.agent_profile.model_endpoint;
        if endpoint.auth_mode != "anthropic_api_key" {
            return Err(HarnessError::Credential {
                credential_ref: endpoint.credential_ref.clone(),
                reason: format!(
                    "the Claude adapter currently supports only anthropic_api_key, not {}",
                    endpoint.auth_mode
                ),
            });
        }
        let credential_dir = ctx.credential_dir.ok_or_else(|| HarnessError::Credential {
            credential_ref: endpoint.credential_ref.clone(),
            reason: "no credential directory available on this worker".into(),
        })?;
        let key = std::fs::read_to_string(credential_dir.join("api_key")).map_err(|e| {
            HarnessError::Credential {
                credential_ref: endpoint.credential_ref.clone(),
                reason: format!("cannot read api_key: {e}"),
            }
        })?;

        let mut tools: Vec<&str> = TOOLS.to_vec();
        match ctx.job.network.web_search.mode {
            WebSearchMode::Disabled => {}
            // Claude Code's web tools cannot be confined to a domain allowlist here; refuse
            // rather than search more widely than the frozen policy permits.
            WebSearchMode::Live
                if ctx.job.network.web_search.domain_scope != WebSearchScope::PublicInternet =>
            {
                return Err(HarnessError::Invalid(
                    "the Claude adapter supports live web search only with domain_scope public_internet".into(),
                ));
            }
            WebSearchMode::Live => tools.extend_from_slice(WEB_TOOLS),
            other => {
                return Err(HarnessError::Invalid(format!(
                    "the Claude adapter supports web_search disabled or live, not {other:?}"
                )));
            }
        }

        let mut args: Vec<String> = vec![
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--dangerously-skip-permissions".into(),
            "--no-session-persistence".into(),
            "--strict-mcp-config".into(),
            "--setting-sources".into(),
            String::new(),
        ];
        if ctx.job.agent_profile.model != "default" {
            args.push("--model".into());
            args.push(ctx.job.agent_profile.model.clone());
        }
        for (key, value) in &ctx.job.agent_profile.model_settings {
            match key.as_str() {
                "effort" => {
                    let effort =
                        value
                            .as_str()
                            .filter(|v| EFFORTS.contains(v))
                            .ok_or_else(|| {
                                HarnessError::Invalid(format!(
                                    "model_settings.effort must be one of {}",
                                    EFFORTS.join(", ")
                                ))
                            })?;
                    args.push("--effort".into());
                    args.push(effort.to_owned());
                }
                "max_budget_usd" => {
                    let budget = value.as_f64().filter(|v| *v > 0.0).ok_or_else(|| {
                        HarnessError::Invalid(
                            "model_settings.max_budget_usd must be a positive number".into(),
                        )
                    })?;
                    args.push("--max-budget-usd".into());
                    args.push(budget.to_string());
                }
                other => {
                    return Err(HarnessError::Invalid(format!(
                        "the Claude adapter does not understand model_settings.{other}"
                    )));
                }
            }
        }
        // `--tools` takes a variadic list, so it comes last; the prompt arrives on stdin.
        args.push("--tools".into());
        args.extend(tools.iter().map(|t| (*t).to_owned()));

        let mut env = common_env(ctx);
        env.push(("ANTHROPIC_API_KEY".into(), key.trim().to_owned()));
        if let Some(base_url) = &endpoint.base_url {
            env.push(("ANTHROPIC_BASE_URL".into(), base_url.clone()));
        }
        env.push(("DISABLE_AUTOUPDATER".into(), "1".into()));
        env.push((
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ));

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
            credential_updates: vec![],
        })
    }

    fn read_transcript(&self, stdout: &[u8]) -> TranscriptFacts {
        let mut facts = TranscriptFacts::default();
        let mut result: Option<serde_json::Value> = None;
        for line in String::from_utf8_lossy(stdout).lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(id) = event.get("session_id").and_then(|v| v.as_str()) {
                facts.session_id = Some(id.to_owned());
            }
            match event.get("type").and_then(|v| v.as_str()) {
                Some("system") if facts.model.is_none() => {
                    facts.model = event
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                Some("assistant")
                    if event.get("parent_tool_use_id").is_none_or(|v| v.is_null()) =>
                {
                    if let Some(model) = event
                        .pointer("/message/model")
                        .and_then(|v| v.as_str())
                        .filter(|m| !m.starts_with('<'))
                    {
                        facts.model = Some(model.to_owned());
                    }
                }
                Some("result") => result = Some(event),
                _ => {}
            }
        }
        // Only the final result event has trustworthy totals: assistant events repeat a message's
        // early usage (output counts stay at their first value), so a stream that stops before
        // its result (timeout, crash) reports no usage rather than an undercount.
        let Some(result) = result else {
            facts.failure = Some("Claude Code ended without a final result".into());
            return facts;
        };
        if result.get("is_error").and_then(|v| v.as_bool()) != Some(false) {
            let subtype = result
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let reason = result
                .get("terminal_reason")
                .or_else(|| result.get("api_error_type"))
                .and_then(|v| v.as_str())
                .unwrap_or(subtype);
            facts.failure = Some(format!("Claude Code reported an error result ({reason})"));
        }
        let totals = match result
            .get("modelUsage")
            .and_then(|v| v.as_object())
            .filter(|m| !m.is_empty())
        {
            // Every model the session used, subagents included, cumulative for the session.
            Some(by_model) => by_model.values().fold(Totals::default(), |totals, usage| {
                totals.add(
                    usage,
                    [
                        "inputTokens",
                        "outputTokens",
                        "cacheReadInputTokens",
                        "cacheCreationInputTokens",
                    ],
                )
            }),
            None => result.get("usage").map_or_else(Totals::default, |usage| {
                Totals::default().add(
                    usage,
                    [
                        "input_tokens",
                        "output_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ],
                )
            }),
        };
        if let (Some(input), Some(output)) = (totals.input, totals.output) {
            // Anthropic's input_tokens excludes cached prefixes; report the full prompt size as
            // input, as other harnesses do, and each cached share separately when reported.
            facts.input_tokens = Some(
                input
                    .saturating_add(totals.cache_read.unwrap_or(0))
                    .saturating_add(totals.cache_creation.unwrap_or(0)),
            );
            facts.output_tokens = Some(output);
            facts.cache_read_input_tokens = totals.cache_read;
            facts.cache_creation_input_tokens = totals.cache_creation;
        }
        facts.cost_usd = result.get("total_cost_usd").and_then(|v| v.as_f64());
        facts
    }
}

/// Summed counters; each stays absent until some model reports it.
#[derive(Default)]
struct Totals {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
}

impl Totals {
    fn add(self, usage: &serde_json::Value, [input, output, read, creation]: [&str; 4]) -> Self {
        let sum = |total: Option<u64>, key: &str| match usage.get(key).and_then(|v| v.as_u64()) {
            Some(n) => Some(total.unwrap_or(0).saturating_add(n)),
            None => total,
        };
        Self {
            input: sum(self.input, input),
            output: sum(self.output, output),
            cache_read: sum(self.cache_read, read),
            cache_creation: sum(self.cache_creation, creation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artidezi_contract::Job;
    use artidezi_contract::common::{ArtifactRef, Outcome};
    use artidezi_contract::job::*;

    fn harness() -> ClaudeHarness {
        ClaudeHarness {
            binary: PathBuf::from("claude"),
        }
    }

    #[test]
    fn result_event_supplies_totals_across_models_and_cost() {
        let facts = harness().read_transcript(
            br#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-opus-5-5"}
{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":9000}},"parent_tool_use_id":null,"session_id":"s-1"}
not json
{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","total_cost_usd":0.4125,"usage":{"input_tokens":1,"output_tokens":1},"modelUsage":{"claude-opus-5-5":{"inputTokens":30,"outputTokens":4000,"cacheReadInputTokens":200000,"cacheCreationInputTokens":20000,"costUSD":0.4},"claude-haiku-5-5":{"inputTokens":10,"outputTokens":100,"cacheReadInputTokens":0,"cacheCreationInputTokens":5000,"costUSD":0.0125}}}
"#,
        );
        assert_eq!(facts.session_id.as_deref(), Some("s-1"));
        assert_eq!(facts.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(facts.input_tokens, Some(30 + 10 + 200_000 + 25_000));
        assert_eq!(facts.output_tokens, Some(4100));
        assert_eq!(facts.cache_read_input_tokens, Some(200_000));
        assert_eq!(facts.cache_creation_input_tokens, Some(25_000));
        assert_eq!(facts.cost_usd, Some(0.4125));
        assert_eq!(facts.failure, None);
    }

    #[test]
    fn without_a_result_there_is_no_usage_and_the_run_failed() {
        let facts = harness().read_transcript(
            br#"{"type":"system","subtype":"init","session_id":"s-2","model":"claude-opus-5-5"}
{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":50}},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":50}},"parent_tool_use_id":null}
"#,
        );
        assert_eq!(facts.session_id.as_deref(), Some("s-2"));
        assert_eq!(facts.input_tokens, None);
        assert_eq!(facts.cost_usd, None);
        assert!(facts.failure.is_some());
    }

    #[test]
    fn unreported_cache_counters_stay_absent() {
        let facts = harness().read_transcript(
            br#"{"type":"result","is_error":false,"usage":{"input_tokens":7,"output_tokens":2},"modelUsage":{}}
"#,
        );
        assert_eq!(facts.input_tokens, Some(7));
        assert_eq!(facts.cache_read_input_tokens, None);
        assert_eq!(facts.cache_creation_input_tokens, None);
        assert_eq!(facts.failure, None);
    }

    fn job(settings: serde_json::Value, web: WebSearchMode, auth: &str) -> Job {
        Job {
            schema_version: 2,
            attempt_id: "att_1".into(),
            task_id: "tsk_1".into(),
            attempt_number: 1,
            prompt_template_version: 1,
            agent_profile: AgentProfile {
                name: "claude".into(),
                harness: Harness::Claude,
                harness_version_constraint: "*".into(),
                model: "claude-opus-5-5".into(),
                model_settings: serde_json::from_value(settings).unwrap(),
                model_endpoint: ModelEndpointRef {
                    auth_mode: auth.into(),
                    credential_ref: "anthropic".into(),
                    base_url: None,
                },
            },
            worker_requirements: WorkerRequirements {
                harness_sandbox: HarnessSandbox::Disabled,
                required_tags: vec![],
            },
            network: NetworkPolicy {
                command_egress: CommandEgress {
                    mode: EgressMode::PublicInternet,
                    allowed_domains: None,
                    denied_domains: vec![],
                    private_network: PrivateNetworkMode::Blocked,
                    allowed_private_endpoints: vec![],
                },
                web_search: WebSearch {
                    mode: web,
                    domain_scope: WebSearchScope::PublicInternet,
                    allowed_domains: None,
                },
            },
            limits: Limits {
                wall_clock_seconds: 60,
                result_json_max_bytes: 1024,
                max_commits_per_repository: 5,
                max_diff_bytes_per_repository: 1024,
                max_commit_message_bytes: 256,
            },
            allowed_outcomes: vec![Outcome::NeedsHuman],
            repositories: vec![],
            task_document: ArtifactRef {
                sha256: "b".repeat(64),
                size: 1,
            },
            previous_attempt: None,
        }
    }

    fn invoke(job: &Job) -> Result<Invocation> {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        std::fs::create_dir(&credentials).unwrap();
        std::fs::write(credentials.join("api_key"), "sk-ant-test\n").unwrap();
        let task = dir.path().join("task.md");
        std::fs::write(&task, "Do the task.").unwrap();
        harness().build_invocation(&InvocationContext {
            job,
            workspace: dir.path(),
            task_document: &task,
            result_path: &dir.path().join("out/result.json"),
            primary_repo_dir: &dir.path().join("repo"),
            home: &dir.path().join("home"),
            tmp: &dir.path().join("tmp"),
            credential_dir: Some(&credentials),
        })
    }

    #[test]
    fn invocation_is_headless_with_the_key_effort_and_fixed_tools() {
        let job = job(
            serde_json::json!({"effort": "medium", "max_budget_usd": 3}),
            WebSearchMode::Disabled,
            "anthropic_api_key",
        );
        let invocation = invoke(&job).unwrap();
        let args = invocation.args.join(" ");
        assert!(args.starts_with("--print --output-format stream-json --verbose --dangerously-skip-permissions --no-session-persistence --strict-mcp-config --setting-sources  "), "{args}");
        assert!(args.contains("--model claude-opus-5-5"), "{args}");
        assert!(args.contains("--effort medium"), "{args}");
        assert!(args.contains("--max-budget-usd 3"), "{args}");
        assert!(
            args.ends_with("--tools Bash Read Edit Write Task"),
            "{args}"
        );
        assert!(
            invocation
                .env
                .contains(&("ANTHROPIC_API_KEY".into(), "sk-ant-test".into()))
        );
        assert_eq!(invocation.stdin.as_deref(), Some(&b"Do the task."[..]));

        let live = invoke(&self::job(
            serde_json::json!({}),
            WebSearchMode::Live,
            "anthropic_api_key",
        ))
        .unwrap();
        assert!(live.args.join(" ").ends_with("Task WebSearch WebFetch"));
        assert!(!live.args.contains(&"--effort".to_owned()));
    }

    #[test]
    fn unsupported_settings_are_refused() {
        for (settings, web, auth) in [
            (
                serde_json::json!({}),
                WebSearchMode::Disabled,
                "claude_subscription_token",
            ),
            (
                serde_json::json!({}),
                WebSearchMode::Cached,
                "anthropic_api_key",
            ),
            (
                serde_json::json!({"effort": "extreme"}),
                WebSearchMode::Disabled,
                "anthropic_api_key",
            ),
            (
                serde_json::json!({"thinking": "high"}),
                WebSearchMode::Disabled,
                "anthropic_api_key",
            ),
        ] {
            assert!(
                invoke(&job(settings.clone(), web, auth)).is_err(),
                "{settings}"
            );
        }
        let mut strict = job(
            serde_json::json!({}),
            WebSearchMode::Disabled,
            "anthropic_api_key",
        );
        strict.worker_requirements.harness_sandbox = HarnessSandbox::Strict;
        assert!(invoke(&strict).is_err());
        let mut scoped = job(
            serde_json::json!({}),
            WebSearchMode::Live,
            "anthropic_api_key",
        );
        scoped.network.web_search.domain_scope = WebSearchScope::DomainAllowlist;
        scoped.network.web_search.allowed_domains = Some(vec!["docs.example.com".into()]);
        assert!(invoke(&scoped).is_err());
    }

    #[test]
    fn a_rejected_key_reports_no_usage() {
        let facts = harness().read_transcript(
            br#"{"type":"system","subtype":"init","session_id":"s-3","model":"claude-opus-5-5"}
{"type":"assistant","message":{"id":"x","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}},"error":"authentication_failed"}
{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","total_cost_usd":0,"usage":{"input_tokens":0,"output_tokens":0},"modelUsage":{}}
"#,
        );
        assert_eq!(facts.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(facts.input_tokens, Some(0));
        assert_eq!(facts.cost_usd, Some(0.0));
        assert!(facts.failure.as_deref().unwrap().contains("api_error"));
    }
}
