// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use artidezi_config::model::DeliveryMode;
use artidezi_config::resolve::{ResolveError, TaskSelection};
use artidezi_config::{LoadError, parse};
use artidezi_contract::common::Outcome;

const VALID: &str = include_str!("fixtures/valid.yaml");

fn problems_of(text: &str) -> Vec<String> {
    match parse(text) {
        Err(LoadError::Invalid(problems)) => problems.into_iter().map(|p| p.0).collect(),
        Err(other) => panic!("expected validation problems, got {other}"),
        Ok(_) => vec![],
    }
}

#[test]
fn fixture_is_valid() {
    let config = parse(VALID).expect("fixture parses and validates");
    assert_eq!(config.projects.len(), 1);
    assert_eq!(config.agent_profiles.len(), 3);
}

#[test]
fn workers_are_listed_with_their_own_tokens() {
    let mut config = parse(VALID).unwrap();
    let pool = config.worker_pools.values_mut().next().unwrap();
    let worker = pool.workers.values().next().unwrap().clone();
    pool.workers.insert("second".into(), worker);
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|problem| problem.to_string().contains("another worker"))
    );

    let pool = config.worker_pools.values_mut().next().unwrap();
    pool.workers.remove("second");
    pool.workers.values_mut().next().unwrap().token_ref = config.instance.admin_token_ref.clone();
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|problem| problem.to_string().contains("admin API"))
    );

    let pool = config.worker_pools.values_mut().next().unwrap();
    pool.workers.clear();
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|problem| problem.to_string().contains("workers must not be empty"))
    );
}

#[test]
fn lost_execution_retries_are_opt_in_and_bounded() {
    let mut config = parse(VALID).unwrap();
    assert!(
        config
            .worker_pools
            .values()
            .all(|p| p.lost_attempt_retries == 0)
    );
    config
        .worker_pools
        .values_mut()
        .next()
        .unwrap()
        .lost_attempt_retries = 2;
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .worker_pools
        .values_mut()
        .next()
        .unwrap()
        .lost_attempt_retries = 4;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("lost_attempt_retries"))
    );
}

#[test]
fn transient_execution_retries_are_opt_in_and_bounded() {
    let mut config = parse(VALID).unwrap();
    assert!(
        config
            .worker_pools
            .values()
            .all(|p| p.transient_failure_retries == 0)
    );
    config
        .worker_pools
        .values_mut()
        .next()
        .unwrap()
        .transient_failure_retries = 5;
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .worker_pools
        .values_mut()
        .next()
        .unwrap()
        .transient_failure_retries = 6;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("transient_failure_retries"))
    );
}

#[test]
fn instance_limits_have_defaults_and_reject_zero() {
    let mut config = artidezi_config::parse(VALID).unwrap();
    assert_eq!(
        config.instance.limits,
        artidezi_config::model::InstanceLimits::default()
    );
    config.instance.limits.git_fetch_timeout_seconds = 0;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("instance.limits"))
    );
}

#[test]
fn artifact_retention_is_opt_in_and_rejects_zero_durations() {
    let mut config = parse(VALID).unwrap();
    assert!(config.instance.artifact_retention.is_none());
    config.instance.artifact_retention = Some(artidezi_config::model::ArtifactRetention {
        completed_task_bundle_retention_seconds: 86400,
        orphan_grace_seconds: 86400,
        cleanup_interval_seconds: 3600,
    });
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .instance
        .artifact_retention
        .as_mut()
        .unwrap()
        .orphan_grace_seconds = 0;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("artifact_retention"))
    );
}

#[test]
fn configured_ui_accounts_validate_grants_and_keep_initial_hash_out_of_output() {
    let hash = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let text = format!(
        "{VALID}\nui_access:\n  schema_version: 1\n  accounts:\n    alice:\n      display_name: Alice\n      initial_password_hash: '{hash}'\n      instance_admin: true\n      grants: {{acme-roles: read}}\n"
    );
    let config = parse(&text).unwrap();
    assert!(!format!("{config:?}").contains(hash));
    assert!(!serde_json::to_string(&config).unwrap().contains(hash));
    assert!(
        problems_of(&text.replace("acme-roles: read", "missing: read"))
            .iter()
            .any(|p| p.contains("unknown project"))
    );
    assert!(
        problems_of(&text.replace("instance_admin: true", "instance_admin: false"))
            .iter()
            .any(|p| p.contains("enabled instance administrator"))
    );
    assert!(
        problems_of(&text.replace(hash, "not-a-hash"))
            .iter()
            .any(|p| p.contains("invalid initial_password_hash"))
    );
    assert!(
        problems_of(&text.replace("m=19456", "m=1048576"))
            .iter()
            .any(|p| p.contains("invalid initial_password_hash"))
    );
    assert!(
        problems_of(&text.replace(hash, "$pbkdf2-sha256$600000$legacy$legacy"))
            .iter()
            .any(|p| p.contains("invalid initial_password_hash"))
    );
    assert!(
        problems_of(&text.replace(
            "ui_access:\n  schema_version: 1",
            "ui_access:\n  schema_version: 2"
        ))
        .iter()
        .any(|p| p.contains("ui_access.schema_version"))
    );
}

#[test]
fn discovery_is_optional_and_handlers_are_explicit_and_ordered() {
    use artidezi_config::model::{PullRequestAction, PullRequestDiscovery, PullRequestHandler};
    let mut config = parse(VALID).unwrap();
    let project_name = config.projects.keys().next().unwrap().clone();
    assert!(config.projects[&project_name].pull_requests.is_none());
    let source_id = config.projects[&project_name]
        .repositories
        .iter()
        .find(|(_, r)| r.agent_access == artidezi_contract::common::RepositoryAccess::ReadWrite)
        .unwrap()
        .0
        .clone();
    let handler = PullRequestHandler {
        safe_verdict_action: Default::default(),
        mode: Default::default(),
        id: "renovate".into(),
        agent_profile: None,
        authors: Some(vec!["renovate[bot]".into()]),
        author_regex: None,
        title_regex: None,
        catch_all: false,
        action: PullRequestAction::Assess,
        wait_for_checks_to_finish: true,
        require_successful_checks: false,
        ci_context: Default::default(),
        instructions: "Assess".into(),
    };
    let mut old_handler = serde_json::to_value(&handler).unwrap();
    old_handler
        .as_object_mut()
        .unwrap()
        .remove("wait_for_checks_to_finish");
    let restored: PullRequestHandler = serde_json::from_value(old_handler).unwrap();
    assert!(restored.wait_for_checks_to_finish);
    config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests = Some(PullRequestDiscovery {
        poll_interval_seconds: 60,
        settle_seconds: 10,
        max_runs_per_pull_request: 3,
        max_active_tasks: 1,
        repositories: vec![source_id],
        ci_log_context: Default::default(),
        handlers: vec![handler.clone()],
    });
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .agent_profile = Some("codex-cheap".into());
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .agent_profile = Some("ghost".into());
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("not in allowed_agent_profiles"))
    );
    config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .agent_profile = None;
    assert!(!handler.matches_pull_request("a-human", "Update dependency to v2"));
    assert!(handler.matches_pull_request("renovate[bot]", "Update dependency to v2"));
    let mut title_scoped = handler.clone();
    title_scoped.title_regex = Some(r"^Update selected-package to v[0-9]+$".into());
    assert!(title_scoped.matches_pull_request("renovate[bot]", "Update selected-package to v2"));
    assert!(!title_scoped.matches_pull_request("renovate[bot]", "Update another-package to v2"));
    let mut unsupported_ci = config.clone();
    unsupported_ci
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .ci_context
        .results = true;
    // The fixture's forge is Forgejo, which reports commit statuses; the fake forge reports none.
    assert!(
        artidezi_config::validate(&unsupported_ci).is_empty(),
        "{:?}",
        artidezi_config::validate(&unsupported_ci)
    );
    for forge in unsupported_ci.forges.values_mut() {
        forge.provider = artidezi_config::model::ForgeProvider::Fake;
    }
    assert!(
        artidezi_config::validate(&unsupported_ci)
            .iter()
            .any(|p| p.0.contains("CI result context requires GitHub or Forgejo"))
    );
    let mut unsafe_logs = config.clone();
    unsafe_logs
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .ci_context
        .logs = true;
    assert!(
        artidezi_config::validate(&unsafe_logs)
            .iter()
            .any(|p| p.0.contains("CI logs require project"))
    );
    let discovery = unsafe_logs
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.ci_log_context.permitted = true;
    discovery.handlers[0].ci_context.results = true;
    let mut missing_selector = config.clone();
    missing_selector
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .authors = None;
    assert!(
        artidezi_config::validate(&missing_selector)
            .iter()
            .any(|p| p.0.contains("exactly one of authors, author_regex"))
    );
    let mut conflicting_selectors = config.clone();
    conflicting_selectors
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0]
        .catch_all = true;
    assert!(
        artidezi_config::validate(&conflicting_selectors)
            .iter()
            .any(|p| p.0.contains("exactly one of authors, author_regex"))
    );
    for pattern in ["", "(", "x*"] {
        let mut invalid_author = config.clone();
        let handler = &mut invalid_author
            .projects
            .get_mut(&project_name)
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .handlers[0];
        handler.authors = None;
        handler.author_regex = Some(pattern.into());
        assert!(
            artidezi_config::validate(&invalid_author)
                .iter()
                .any(|p| p.0.contains("author_regex must be a valid regex")),
            "{pattern:?}"
        );
    }
    for pattern in ["", "("] {
        let mut invalid_title = config.clone();
        invalid_title
            .projects
            .get_mut(&project_name)
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .handlers[0]
            .title_regex = Some(pattern.into());
        assert!(
            artidezi_config::validate(&invalid_title)
                .iter()
                .any(|p| p.0.contains("title_regex must be a nonempty valid regex"))
        );
    }
    let mut title_catch_all = config.clone();
    let title_catch_all_handler = &mut title_catch_all
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0];
    title_catch_all_handler.authors = None;
    title_catch_all_handler.catch_all = true;
    title_catch_all_handler.title_regex = Some("selected-package".into());
    assert!(
        artidezi_config::validate(&title_catch_all)
            .iter()
            .any(|p| p.0.contains("title_regex requires authors or author_regex"))
    );
    let discovery = config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap();
    discovery.handlers[0].authors = None;
    discovery.handlers[0].catch_all = true;
    discovery.handlers.push(PullRequestHandler {
        safe_verdict_action: Default::default(),
        mode: Default::default(),
        id: "unreachable".into(),
        ..handler
    });
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("unreachable"))
    );
    config
        .projects
        .get_mut(&project_name)
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers
        .pop();
    let duplicate = config.projects[&project_name].clone();
    config.projects.insert("overlapping".into(), duplicate);
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("already watched"))
    );
}

#[test]
fn unknown_field_is_a_parse_error() {
    let text = VALID.replace("data_dir:", "data_dirr:");
    assert!(matches!(parse(&text), Err(LoadError::Parse(_))));
}

#[test]
fn dangling_references_are_reported() {
    let text = VALID.replace(
        "model_endpoint: openai-chatgpt-personal",
        "model_endpoint: nope",
    );
    let problems = problems_of(&text);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("model_endpoint \"nope\" does not exist")),
        "{problems:?}"
    );
}

#[test]
fn harness_and_endpoint_provider_must_match() {
    let text = VALID.replace("    harness: codex\n", "    harness: claude\n");
    let problems = problems_of(&text);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("cannot use a Openai endpoint")),
        "{problems:?}"
    );
}

#[test]
fn network_allowlist_needs_domains() {
    let text = VALID.replace(
        "mode: public_internet\n        denied_domains",
        "mode: domain_allowlist\n        denied_domains",
    );
    let problems = problems_of(&text);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("allowed_domains is mandatory")),
        "{problems:?}"
    );
}

#[test]
fn defaults_must_be_within_allowed_lists() {
    let text = VALID.replace("default_delivery_mode: merge_request\n", "default_delivery_mode: direct_push_fast_forward\n")
        .replace("allowed_delivery_modes: [merge_request, merge_request_with_automerge, direct_push_fast_forward]", "allowed_delivery_modes: [merge_request]");
    let problems = problems_of(&text);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not in allowed_delivery_modes")),
        "{problems:?}"
    );
}

#[test]
fn resolution_freezes_policy_and_respects_allowlists() {
    let config = parse(VALID).unwrap();
    let outcomes = config.projects["acme-roles"].manual_task_outcomes();
    assert_eq!(
        outcomes,
        vec![Outcome::Proposal, Outcome::NeedsHuman, Outcome::Failed]
    );

    let policy = config
        .resolve_project("acme-roles", &TaskSelection::default(), outcomes.clone())
        .unwrap();
    assert_eq!(policy.delivery_mode, DeliveryMode::MergeRequest);
    assert_eq!(policy.agent_profile.name, "codex-personal");
    assert!(
        policy
            .agent_instructions
            .contains("Inspect only the components relevant to the task")
    );
    assert_eq!(
        policy.agent_profile.model_endpoint.credential_ref,
        "personal-worker-codex"
    );
    assert_eq!(policy.repositories.len(), 2);
    assert_eq!(policy.digest().len(), 64);

    let selection = TaskSelection {
        agent_profile: Some("fake".into()),
        delivery_mode: Some(DeliveryMode::DirectPushFastForward),
    };
    let policy = config
        .resolve_project("acme-roles", &selection, outcomes.clone())
        .unwrap();
    assert_eq!(
        policy.agent_profile.harness,
        artidezi_contract::job::Harness::Fake
    );
    assert_eq!(policy.delivery_mode, DeliveryMode::DirectPushFastForward);

    let selection = TaskSelection {
        agent_profile: Some("ghost".into()),
        delivery_mode: None,
    };
    assert_eq!(
        config
            .resolve_project("acme-roles", &selection, outcomes)
            .unwrap_err(),
        ResolveError::ProfileNotAllowed("ghost".into())
    );
}

#[test]
fn github_requires_explicit_installation_scope_and_explicit_merge_policy() {
    use artidezi_config::model::{ForgeProvider, GithubApp};
    let mut config = parse(VALID).unwrap();
    let forge = config.forges.get_mut("forge-acme").unwrap();
    forge.provider = ForgeProvider::Github;
    forge.api_base_url = "https://api.github.com".into();
    forge.github_app = Some(GithubApp {
        client_id: "Iv.fixture".into(),
        installation_id: 42,
        account: "acme".into(),
    });
    let project = config.projects.values_mut().next().unwrap();
    project.effects.publish_adaptation_merge_requests = false;
    project.effects.merge_source_merge_request_on_safe_verdict = false;
    for repo in project.repositories.values_mut() {
        repo.tagging = None;
    }
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
    assert!(config.manual_tasks_available(config.projects.values().next().unwrap()));
    for variant in 0..5 {
        let mut bad = config.clone();
        match variant {
            0 => bad.forges.get_mut("forge-acme").unwrap().github_app = None,
            1 => {
                bad.forges.get_mut("forge-acme").unwrap().api_base_url = "http://127.0.0.1:1".into()
            }
            2 => {
                bad.forges
                    .get_mut("forge-acme")
                    .unwrap()
                    .github_app
                    .as_mut()
                    .unwrap()
                    .account = "other".into()
            }
            3 => {
                bad.projects
                    .values_mut()
                    .next()
                    .unwrap()
                    .effects
                    .merge_source_merge_request_on_safe_verdict = true
            }
            4 => bad.forges.get_mut("forge-acme").unwrap().credential_ref = "../key".into(),
            _ => unreachable!(),
        }
        assert!(
            !artidezi_config::validate(&bad).is_empty(),
            "variant {variant}"
        );
    }
}

#[test]
fn publication_requires_both_a_handler_opt_in_and_project_effect() {
    use artidezi_config::model::{
        PullRequestAction, PullRequestDiscovery, PullRequestHandler, PullRequestMode,
    };
    let mut config = parse(VALID).unwrap();
    let project = config.projects.values_mut().next().unwrap();
    let repo = project
        .repositories
        .iter()
        .find(|(_, r)| r.agent_access == artidezi_contract::common::RepositoryAccess::ReadWrite)
        .unwrap()
        .0
        .clone();
    project.effects.merge_source_merge_request_on_safe_verdict = false;
    project.pull_requests = Some(PullRequestDiscovery {
        poll_interval_seconds: 60,
        settle_seconds: 0,
        max_runs_per_pull_request: 3,
        max_active_tasks: 1,
        repositories: vec![repo],
        ci_log_context: Default::default(),
        handlers: vec![PullRequestHandler {
            safe_verdict_action: Default::default(),
            id: "updates".into(),
            agent_profile: None,
            authors: Some(vec!["updater".into()]),
            author_regex: None,
            title_regex: None,
            catch_all: false,
            action: PullRequestAction::Assess,
            mode: PullRequestMode::Publish,
            wait_for_checks_to_finish: true,
            require_successful_checks: false,
            ci_context: Default::default(),
            instructions: "Assess compatibility".into(),
        }],
    });
    project.effects.publish_adaptation_merge_requests = true;
    assert!(artidezi_config::validate(&config).is_empty());
    config
        .projects
        .values_mut()
        .next()
        .unwrap()
        .effects
        .publish_adaptation_merge_requests = false;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("publish mode requires"))
    );
    let p = config.projects.values_mut().next().unwrap();
    p.effects.publish_adaptation_merge_requests = true;
    p.effects.merge_source_merge_request_on_safe_verdict = true;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("automatic source merging"))
    );
}

/// A valid project whose only handler merges `renovate[bot]` pull requests on a GitHub forge.
fn github_merge_handler_config() -> artidezi_config::Config {
    use artidezi_config::model::*;
    let mut config = parse(VALID).unwrap();
    let forge = config.forges.get_mut("forge-acme").unwrap();
    forge.provider = ForgeProvider::Github;
    forge.api_base_url = "https://api.github.com".into();
    forge.github_app = Some(GithubApp {
        client_id: "Iv.fixture".into(),
        installation_id: 42,
        account: "acme".into(),
    });
    let project = config.projects.values_mut().next().unwrap();
    for repo in project.repositories.values_mut() {
        repo.tagging = None;
    }
    let repo = project
        .repositories
        .iter()
        .find(|(_, r)| r.agent_access == artidezi_contract::common::RepositoryAccess::ReadWrite)
        .unwrap()
        .0
        .clone();
    project.effects.publish_adaptation_merge_requests = true;
    project.effects.merge_source_merge_request_on_safe_verdict = true;
    project.pull_requests = Some(PullRequestDiscovery {
        poll_interval_seconds: 60,
        settle_seconds: 0,
        max_runs_per_pull_request: 3,
        max_active_tasks: 1,
        repositories: vec![repo],
        ci_log_context: Default::default(),
        handlers: vec![PullRequestHandler {
            id: "renovate".into(),
            agent_profile: None,
            authors: Some(vec!["renovate[bot]".into()]),
            author_regex: None,
            title_regex: None,
            catch_all: false,
            mode: PullRequestMode::Publish,
            action: PullRequestAction::Assess,
            safe_verdict_action: SafeVerdictAction::Merge,
            wait_for_checks_to_finish: true,
            require_successful_checks: false,
            ci_context: Default::default(),
            instructions: "Assess".into(),
        }],
    });
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
    config
}

#[test]
fn source_actions_are_opt_in_and_reject_unscoped_and_dry_run_authority() {
    use artidezi_config::model::*;
    let mut config = github_merge_handler_config();
    for variant in 0..5 {
        let mut bad = config.clone();
        let p = bad.projects.values_mut().next().unwrap();
        let h = &mut p.pull_requests.as_mut().unwrap().handlers[0];
        match variant {
            0 => h.authors = None,
            1 => h.mode = PullRequestMode::DryRun,
            2 => h.authors = Some(vec![]),
            3 => p.effects.merge_source_merge_request_on_safe_verdict = false,
            4 => p.effects.publish_adaptation_merge_requests = false,
            _ => unreachable!(),
        }
        assert!(!artidezi_config::validate(&bad).is_empty(), "{variant}");
    }
    let p = config.projects.values_mut().next().unwrap();
    p.effects.merge_source_merge_request_on_safe_verdict = false;
    p.pull_requests.as_mut().unwrap().handlers[0].safe_verdict_action = SafeVerdictAction::Approve;
    assert!(artidezi_config::validate(&config).is_empty());
}

#[test]
fn a_merge_handler_may_merge_any_author_it_names() {
    // Whether an author is a bot or a trusted person is the operator's decision, not a login shape.
    let mut config = github_merge_handler_config();
    for author in ["deps-bot", "a-human", "acme-updates[bot]"] {
        config
            .projects
            .values_mut()
            .next()
            .unwrap()
            .pull_requests
            .as_mut()
            .unwrap()
            .handlers[0]
            .authors = Some(vec![author.into()]);
        assert!(
            artidezi_config::validate(&config).is_empty(),
            "{author}: {:?}",
            artidezi_config::validate(&config)
        );
    }
}

#[test]
fn a_merge_handler_may_name_its_authors_by_pattern_or_take_every_author() {
    let mut config = github_merge_handler_config();
    let handler = &mut config
        .projects
        .values_mut()
        .next()
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0];
    handler.authors = None;
    handler.author_regex = Some(r"[a-z-]+\[bot\]".into());
    handler.title_regex = Some("^Update ".into());
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
    let handler = &config
        .projects
        .values()
        .next()
        .unwrap()
        .pull_requests
        .as_ref()
        .unwrap()
        .handlers[0];
    // The pattern covers the whole login, ignoring case, like an exact author.
    assert!(handler.matches_pull_request("Renovate[bot]", "Update x"));
    assert!(!handler.matches_pull_request("renovate[bot]-impostor", "Update x"));
    assert!(!handler.matches_pull_request("not renovate[bot]", "Update x"));
    assert!(!handler.matches_pull_request("renovate[bot]", "Bump x"));
    let handler = &mut config
        .projects
        .values_mut()
        .next()
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0];
    handler.author_regex = None;
    handler.title_regex = None;
    handler.catch_all = true;
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
}

#[test]
fn forgejo_sources_may_be_approved_or_merged() {
    use artidezi_config::model::{ForgeProvider, SafeVerdictAction};
    let mut config = github_merge_handler_config();
    let forge = config.forges.get_mut("forge-acme").unwrap();
    forge.provider = ForgeProvider::Forgejo;
    forge.api_base_url = "https://code.example.org".into();
    forge.github_app = None;
    let handler = &mut config
        .projects
        .values_mut()
        .next()
        .unwrap()
        .pull_requests
        .as_mut()
        .unwrap()
        .handlers[0];
    handler.authors = Some(vec!["renovate-bot".into()]);
    assert!(
        artidezi_config::validate(&config).is_empty(),
        "{:?}",
        artidezi_config::validate(&config)
    );
    let p = config.projects.values_mut().next().unwrap();
    p.effects.merge_source_merge_request_on_safe_verdict = false;
    p.pull_requests.as_mut().unwrap().handlers[0].safe_verdict_action = SafeVerdictAction::Approve;
    assert!(artidezi_config::validate(&config).is_empty());
    // The fake forge has no source actions.
    config.forges.get_mut("forge-acme").unwrap().provider = ForgeProvider::Fake;
    assert!(
        artidezi_config::validate(&config)
            .iter()
            .any(|p| p.0.contains("GitHub or Forgejo"))
    );
}

#[test]
fn the_former_merge_effect_name_is_still_accepted() {
    let config = parse(&VALID.replace(
        "merge_source_merge_request_on_safe_verdict",
        "merge_renovate_merge_request_on_safe_verdict",
    ))
    .unwrap();
    assert_eq!(config, parse(VALID).unwrap());
    let effects: artidezi_config::model::Effects = serde_json::from_str(
        r#"{"merge_renovate_merge_request_on_safe_verdict": true, "publish_adaptation_merge_requests": true}"#,
    )
    .unwrap();
    assert!(effects.merge_source_merge_request_on_safe_verdict);
}

#[test]
fn shipped_examples_are_valid() {
    for example in [
        include_str!("../../../etc/example.yaml"),
        include_str!("../../../etc/github-dry-run.yaml"),
    ] {
        artidezi_config::parse(example).expect("example should be valid");
    }
}
