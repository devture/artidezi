// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use argon2::password_hash::phc::PasswordHash;
use artidezi_contract::common::RepositoryAccess;
use artidezi_contract::job::{EgressMode, NetworkPolicy, PrivateNetworkMode, WebSearchScope};
use regex::Regex;

use crate::model::{CONFIG_SCHEMA_VERSION, Config, DeliveryMode, ForgeProvider, ModelProvider};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Problem(pub String);

fn problem(msg: impl Into<String>) -> Problem {
    Problem(msg.into())
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !name.starts_with('.')
}

/// Accept only the bounded Argon2id profile used for new browser credentials.
/// This also prevents a stored PHC string from requesting unbounded memory at login.
pub fn valid_ui_initial_hash(value: &str) -> bool {
    let Ok(hash) = PasswordHash::new(value) else {
        return false;
    };
    hash.algorithm.as_str() == "argon2id"
        && hash.version == Some(19)
        && hash.params.iter().count() == 3
        && hash.params.get_decimal("m") == Some(19 * 1024)
        && hash.params.get_decimal("t") == Some(2)
        && hash.params.get_decimal("p") == Some(1)
        && hash.salt.as_ref().is_some_and(|salt| salt.len() == 16)
        && hash
            .hash
            .as_ref()
            .is_some_and(|output| output.as_bytes().len() == 32)
}

pub fn validate(config: &Config) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut watched = BTreeSet::new();

    if config.schema_version != CONFIG_SCHEMA_VERSION {
        problems.push(problem(format!(
            "schema_version {} is not the supported {CONFIG_SCHEMA_VERSION}",
            config.schema_version
        )));
    }
    if config.instance.name.trim().is_empty() {
        problems.push(problem("instance.name must not be empty"));
    }
    if let Some(retention) = &config.instance.artifact_retention
        && (retention.completed_task_bundle_retention_seconds == 0
            || retention.orphan_grace_seconds == 0
            || retention.cleanup_interval_seconds == 0)
    {
        problems.push(problem(
            "instance.artifact_retention durations must be positive",
        ));
    }
    let limits = &config.instance.limits;
    if limits.max_upload_bytes == 0
        || limits.max_attempt_upload_bytes == 0
        || limits.max_concurrent_uploads == 0
        || limits.max_concurrent_inspections == 0
        || limits.max_bundle_bytes == 0
        || limits.git_fetch_timeout_seconds == 0
    {
        problems.push(problem("instance.limits values must be positive"));
    }
    if config.instance.bot_identity.name.trim().is_empty()
        || !config.instance.bot_identity.email.contains('@')
    {
        problems.push(problem("instance.bot_identity needs a name and an email"));
    }
    if !config.instance.data_dir.starts_with('/') {
        problems.push(problem("instance.data_dir must be an absolute path"));
    }
    if !is_plain_name(&config.instance.admin_token_ref) {
        problems.push(problem(
            "instance.admin_token_ref must be a plain credential name",
        ));
    }

    for (name, endpoint) in &config.model_endpoints {
        if !is_plain_name(name) {
            problems.push(problem(format!(
                "model endpoint name {name:?} is not a plain name"
            )));
        }
        if !endpoint
            .provider
            .auth_modes()
            .contains(&endpoint.auth_mode.as_str())
        {
            problems.push(problem(format!(
                "model endpoint {name}: auth_mode {:?} is not valid for provider {:?}; expected one of {:?}",
                endpoint.auth_mode,
                endpoint.provider,
                endpoint.provider.auth_modes()
            )));
        }
        if endpoint.credential_ref.trim().is_empty() {
            problems.push(problem(format!(
                "model endpoint {name}: credential_ref must not be empty"
            )));
        }
        if endpoint.provider == ModelProvider::None && endpoint.base_url.is_some() {
            problems.push(problem(format!(
                "model endpoint {name}: provider none cannot have a base_url"
            )));
        }
    }

    let mut worker_token_refs =
        std::collections::BTreeSet::from([&config.instance.admin_token_ref]);
    for (name, pool) in &config.worker_pools {
        if pool.transient_failure_retries > 5 {
            problems.push(problem(format!(
                "worker pool {name}: transient_failure_retries must be at most 5"
            )));
        }
        if pool.lost_attempt_retries > 3 {
            problems.push(problem(format!(
                "worker pool {name}: lost_attempt_retries must be at most 3"
            )));
        }
        if !is_plain_name(name) {
            problems.push(problem(format!(
                "worker pool name {name:?} is not a plain name"
            )));
        }
        if pool.harnesses.is_empty() {
            problems.push(problem(format!(
                "worker pool {name}: harnesses must not be empty"
            )));
        }
        if pool.workers.is_empty() {
            problems.push(problem(format!(
                "worker pool {name}: workers must not be empty"
            )));
        }
        for (id, worker) in &pool.workers {
            if id.len() > 128 || !is_plain_name(id) {
                problems.push(problem(format!(
                    "worker pool {name}: worker id {id:?} is not a plain name of at most 128 characters"
                )));
            }
            if !is_plain_name(&worker.token_ref) {
                problems.push(problem(format!(
                    "worker {name}/{id}: token_ref must be a plain credential name"
                )));
            }
            if !worker_token_refs.insert(&worker.token_ref) {
                problems.push(problem(format!(
                    "worker {name}/{id}: token_ref {:?} is already used by the admin API or another worker",
                    worker.token_ref
                )));
            }
        }
    }

    for (name, profile) in &config.agent_profiles {
        if !is_plain_name(name) {
            problems.push(problem(format!(
                "agent profile name {name:?} is not a plain name"
            )));
        }
        match config.model_endpoints.get(&profile.model_endpoint) {
            None => problems.push(problem(format!(
                "agent profile {name}: model_endpoint {:?} does not exist",
                profile.model_endpoint
            ))),
            Some(endpoint) if !endpoint.provider.serves(profile.harness) => {
                problems.push(problem(format!(
                    "agent profile {name}: harness {:?} cannot use a {:?} endpoint",
                    profile.harness, endpoint.provider
                )));
            }
            Some(_) => {}
        }
        match config.worker_pools.get(&profile.worker_pool) {
            None => problems.push(problem(format!(
                "agent profile {name}: worker_pool {:?} does not exist",
                profile.worker_pool
            ))),
            Some(pool) if !pool.harnesses.contains(&profile.harness) => {
                problems.push(problem(format!(
                    "agent profile {name}: worker pool {} does not provide harness {:?}",
                    profile.worker_pool, profile.harness
                )));
            }
            Some(_) => {}
        }
        if profile.harness_version_constraint.trim().is_empty() {
            problems.push(problem(format!(
                "agent profile {name}: harness_version_constraint must not be empty"
            )));
        } else if semver::VersionReq::parse(&profile.harness_version_constraint).is_err() {
            problems.push(problem(format!(
                "agent profile {name}: harness_version_constraint is not a valid version requirement"
            )));
        }
    }

    for (name, forge) in &config.forges {
        if !is_plain_name(name) {
            problems.push(problem(format!("forge name {name:?} is not a plain name")));
        }
        if !(forge.api_base_url.starts_with("https://")
            || forge.api_base_url.starts_with("http://"))
        {
            problems.push(problem(format!(
                "forge {name}: api_base_url must be an http(s) URL"
            )));
        }
        if let Some(url) = &forge.web_base_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            problems.push(problem(format!(
                "forge {name}: web_base_url must be an http(s) URL"
            )));
        }
        if forge.credential_ref.trim().is_empty() || forge.bot_login.trim().is_empty() {
            problems.push(problem(format!(
                "forge {name}: credential_ref and bot_login are mandatory"
            )));
        }
        if forge.provider == ForgeProvider::Github {
            if forge.api_base_url.trim_end_matches('/') != "https://api.github.com"
                || forge
                    .web_base_url
                    .as_deref()
                    .is_some_and(|u| u.trim_end_matches('/') != "https://github.com")
            {
                problems.push(problem(format!(
                    "forge {name}: GitHub requires https://api.github.com and https://github.com"
                )));
            }
            match &forge.github_app {
                Some(app) if is_plain_name(&app.client_id) && app.installation_id > 0
                    && is_plain_name(&app.account) && !app.account.contains('.') => {}
                _ => problems.push(problem(format!("forge {name}: github_app needs client_id, a positive installation_id and installation account"))),
            }
            if !is_plain_name(&forge.credential_ref) || !forge.bot_login.ends_with("[bot]") {
                problems.push(problem(format!("forge {name}: GitHub needs a plain PEM credential_ref and App bot_login ending in [bot]")));
            }
        } else if forge.github_app.is_some() {
            problems.push(problem(format!(
                "forge {name}: github_app is only valid for GitHub"
            )));
        }
    }

    if let Some(ui) = &config.ui_access {
        if ui.schema_version != 1 {
            problems.push(problem("ui_access.schema_version must be 1"));
        }
        if !ui
            .accounts
            .values()
            .any(|account| account.instance_admin && !account.disabled)
        {
            problems.push(problem("ui_access needs an enabled instance administrator"));
        }
        for (id, account) in &ui.accounts {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                problems.push(problem(format!(
                    "ui_access account {id:?} has an invalid ID"
                )));
            }
            if account.display_name.trim().is_empty()
                || account.display_name.chars().count() > 128
                || account.display_name.chars().any(char::is_control)
            {
                problems.push(problem(format!(
                    "ui_access account {id:?} has an invalid display_name"
                )));
            }
            if !valid_ui_initial_hash(&account.initial_password_hash) {
                problems.push(problem(format!(
                    "ui_access account {id:?} has an invalid initial_password_hash"
                )));
            }
            for project in account.grants.keys() {
                if project != "*" && !config.projects.contains_key(project) {
                    problems.push(problem(format!(
                        "ui_access account {id:?} grants unknown project {project:?}"
                    )));
                }
            }
        }
    }

    for (name, project) in &config.projects {
        for tag in &project.required_worker_tags {
            if !artidezi_contract::validate::is_plain_id(tag) {
                problems.push(problem(format!(
                    "projects.{name}.required_worker_tags: invalid tag {tag:?}"
                )));
            }
        }
        let ctx = format!("project {name}");
        for (id, repo) in &project.repositories {
            if let Some(forge) = config.forges.get(&repo.forge)
                && forge.provider == ForgeProvider::Github
            {
                let valid = repo.remote.split_once('/').is_some_and(|(owner, name)| {
                    is_plain_name(owner)
                        && is_plain_name(name)
                        && forge
                            .github_app
                            .as_ref()
                            .is_some_and(|app| owner.eq_ignore_ascii_case(&app.account))
                });
                if !valid {
                    problems.push(problem(format!("{ctx}: GitHub repository {id} must be account/name within its configured installation")));
                }
                if repo.tagging.is_some() {
                    problems.push(problem(format!("{ctx}: GitHub tagging is not supported")));
                }
            }
        }
        if let Some(discovery) = &project.pull_requests {
            for handler in &discovery.handlers {
                if handler.mode == crate::model::PullRequestMode::Publish
                    && (handler.action != crate::model::PullRequestAction::Assess
                        || !project.effects.publish_adaptation_merge_requests)
                {
                    problems.push(problem(format!(
                        "{ctx}: publish mode requires assess and publish_adaptation_merge_requests"
                    )));
                }
            }
        }
        // Formal approvals can activate another bot's automerge: treat them as delivery authority.
        if project.effects.merge_source_merge_request_on_safe_verdict
            && !project.pull_requests.as_ref().is_some_and(|d| {
                d.handlers
                    .iter()
                    .any(|h| h.safe_verdict_action == crate::model::SafeVerdictAction::Merge)
            })
        {
            problems.push(problem(format!(
                "{ctx}: automatic source merging requires an explicit merge handler"
            )));
        }
        if project.effects.merge_adaptation_merge_requests
            && !project.pull_requests.as_ref().is_some_and(|d| {
                d.handlers
                    .iter()
                    .any(|h| h.adaptation_action == crate::model::AdaptationAction::Merge)
            })
        {
            problems.push(problem(format!(
                "{ctx}: merge_adaptation_merge_requests requires a handler with adaptation_action: merge"
            )));
        }
        if let Some(discovery) = &project.pull_requests {
            for handler in &discovery.handlers {
                use crate::model::{PullRequestAction, PullRequestMode, SafeVerdictAction};
                if handler.adaptation_action == crate::model::AdaptationAction::Merge {
                    if handler.mode != PullRequestMode::Publish
                        || handler.action != PullRequestAction::Assess
                        || !project.effects.merge_adaptation_merge_requests
                        || project.draft_merge_requests
                    {
                        problems.push(problem(format!(
                            "{ctx}: handler {} adaptation_action: merge requires a publish assessment, the project merge_adaptation_merge_requests effect and draft_merge_requests: false",
                            handler.id
                        )));
                    }
                    // Adaptations may change any writable repository, not only the watched one.
                    if project.repositories.values().any(|r| {
                        r.agent_access == RepositoryAccess::ReadWrite
                            && config.forges.get(&r.forge).is_none_or(|f| {
                                !matches!(
                                    f.provider,
                                    ForgeProvider::Github | ForgeProvider::Forgejo
                                )
                            })
                    }) {
                        problems.push(problem(format!(
                            "{ctx}: handler {} adaptation_action: merge requires every writable repository on GitHub or Forgejo",
                            handler.id
                        )));
                    }
                }
                if let Some(profile) = &handler.agent_profile
                    && !project.allowed_agent_profiles.contains(profile)
                {
                    problems.push(problem(format!(
                        "{ctx}: handler {:?} agent profile {profile:?} is not in allowed_agent_profiles",
                        handler.id
                    )));
                }
                if handler.safe_verdict_checks == crate::model::SafeVerdictChecks::PassingOrNone {
                    if handler.safe_verdict_action == SafeVerdictAction::Comment {
                        problems.push(problem(format!(
                            "{ctx}: handler {} safe_verdict_checks: passing_or_none only applies to safe_verdict_action approve or merge",
                            handler.id
                        )));
                    }
                    // Without checks this gate refuses every comment review and adaptation.
                    if handler.require_successful_checks {
                        problems.push(problem(format!(
                            "{ctx}: handler {} safe_verdict_checks: passing_or_none requires require_successful_checks: false",
                            handler.id
                        )));
                    }
                }
                if handler.safe_verdict_action == SafeVerdictAction::Comment {
                    continue;
                }
                if handler.mode != PullRequestMode::Publish
                    || handler.action != PullRequestAction::Assess
                {
                    problems.push(problem(format!(
                        "{ctx}: source approval/merge requires a publish assessment"
                    )));
                }
                if discovery.repositories.iter().any(|id| {
                    project.repositories.get(id).is_none_or(|r| {
                        r.agent_access != RepositoryAccess::ReadWrite
                            || config.forges.get(&r.forge).is_none_or(|f| {
                                !matches!(
                                    f.provider,
                                    ForgeProvider::Github | ForgeProvider::Forgejo
                                )
                            })
                    })
                }) {
                    problems.push(problem(format!("{ctx}: source approval/merge requires writable GitHub or Forgejo source repositories")));
                }
                if handler.safe_verdict_action == SafeVerdictAction::Merge
                    && !project.effects.merge_source_merge_request_on_safe_verdict
                {
                    problems.push(problem(format!(
                        "{ctx}: source merge requires the project merge effect"
                    )));
                }
            }
        }
        if project
            .allowed_delivery_modes
            .contains(&DeliveryMode::Shadow)
        {
            problems.push(problem(format!(
                "{ctx}: shadow is reserved for PR discovery, not manual delivery"
            )));
        }
        if let Some(discovery) = &project.pull_requests {
            if discovery.poll_interval_seconds < 10
                || discovery.max_active_tasks == 0
                || discovery.max_runs_per_pull_request == 0
                || discovery.repositories.is_empty()
                || discovery.handlers.is_empty()
            {
                problems.push(problem(format!("{ctx}: PR discovery needs an interval >= 10 seconds, positive limits, repositories and handlers")));
            }
            let mut ids = BTreeSet::new();
            let mut catch_all = false;
            for handler in &discovery.handlers {
                if !is_plain_name(&handler.id) || !ids.insert(&handler.id) {
                    problems.push(problem(format!(
                        "{ctx}: PR handler IDs must be unique plain names"
                    )));
                }
                if catch_all {
                    problems.push(problem(format!(
                        "{ctx}: PR handler {} is unreachable after a catch-all",
                        handler.id
                    )));
                }
                catch_all = handler.catch_all;
                let author_selectors = usize::from(handler.catch_all)
                    + usize::from(handler.authors.is_some())
                    + usize::from(handler.author_regex.is_some());
                if author_selectors != 1 {
                    problems.push(problem(format!("{ctx}: PR handler {} needs exactly one of authors, author_regex or explicit catch_all: true", handler.id)));
                }
                if handler.authors.as_ref().is_some_and(|authors| {
                    authors.is_empty() || authors.iter().any(|a| a.trim().is_empty())
                }) {
                    problems.push(problem(format!("{ctx}: PR handler authors must be nonempty; use catch_all: true for an explicit catch-all")));
                }
                if let Some(pattern) = &handler.author_regex
                    && crate::model::author_pattern(pattern)
                        .ok()
                        .is_none_or(|r| r.is_match(""))
                {
                    problems.push(problem(format!(
                        "{ctx}: PR handler {} author_regex must be a valid regex that matches no empty login",
                        handler.id
                    )));
                }
                if let Some(pattern) = &handler.title_regex {
                    if pattern.is_empty() || Regex::new(pattern).is_err() {
                        problems.push(problem(format!(
                            "{ctx}: PR handler {} title_regex must be a nonempty valid regex",
                            handler.id
                        )));
                    }
                    if handler.catch_all {
                        problems.push(problem(format!(
                            "{ctx}: PR handler {} title_regex requires authors or author_regex, not catch_all",
                            handler.id
                        )));
                    }
                }
                if handler.ci_context.results {
                    if handler.action != crate::model::PullRequestAction::Assess {
                        problems.push(problem(format!(
                            "{ctx}: PR handler {} CI context is only useful for assessments",
                            handler.id
                        )));
                    }
                    if discovery.repositories.iter().any(|id| {
                        project.repositories.get(id).is_some_and(|repo| {
                            config.forges.get(&repo.forge).is_some_and(|forge| {
                                !matches!(
                                    forge.provider,
                                    ForgeProvider::Github | ForgeProvider::Forgejo
                                )
                            })
                        })
                    }) {
                        problems.push(problem(format!(
                            "{ctx}: PR handler {} CI result context requires GitHub or Forgejo source repositories",
                            handler.id
                        )));
                    }
                }
                if handler.ci_context.logs
                    && (!discovery.ci_log_context.permitted || !handler.ci_context.results)
                {
                    problems.push(problem(format!(
                        "{ctx}: PR handler {} CI logs require project ci_log_context.permitted and handler ci_context.results",
                        handler.id
                    )));
                }
            }
            let mut repos = BTreeSet::new();
            for id in &discovery.repositories {
                if !repos.insert(id) {
                    problems.push(problem(format!("{ctx}: duplicate watched repository {id}")));
                }
                let Some(repo) = project.repositories.get(id) else {
                    problems.push(problem(format!(
                        "{ctx}: watched repository {id} does not exist"
                    )));
                    continue;
                };
                if repo.agent_access != RepositoryAccess::ReadWrite {
                    problems.push(problem(format!(
                        "{ctx}: assessment source {id} must be read_write"
                    )));
                }
                if let Some(forge) = config.forges.get(&repo.forge) {
                    let key = (
                        forge.api_base_url.trim_end_matches('/').to_lowercase(),
                        repo.remote.to_lowercase(),
                        repo.branch.clone(),
                    );
                    if !watched.insert(key) {
                        problems.push(problem(format!(
                            "{ctx}: this remote/branch is already watched by another project"
                        )));
                    }
                }
            }
        }
        if !is_plain_name(name) {
            problems.push(problem(format!(
                "project name {name:?} is not a plain name"
            )));
        }
        if project.description.trim().is_empty() {
            problems.push(problem(format!("{ctx}: description must not be empty")));
        }
        if project.allowed_agent_profiles.is_empty() {
            problems.push(problem(format!(
                "{ctx}: allowed_agent_profiles must not be empty"
            )));
        }
        if !project
            .allowed_agent_profiles
            .contains(&project.default_agent_profile)
        {
            problems.push(problem(format!(
                "{ctx}: default_agent_profile {:?} is not in allowed_agent_profiles",
                project.default_agent_profile
            )));
        }
        for profile in &project.allowed_agent_profiles {
            if !config.agent_profiles.contains_key(profile) {
                problems.push(problem(format!(
                    "{ctx}: agent profile {profile:?} does not exist"
                )));
            }
        }
        if project.allowed_delivery_modes.is_empty() {
            problems.push(problem(format!(
                "{ctx}: allowed_delivery_modes must not be empty"
            )));
        }
        if !project
            .allowed_delivery_modes
            .contains(&project.default_delivery_mode)
        {
            problems.push(problem(format!(
                "{ctx}: default_delivery_mode {:?} is not in allowed_delivery_modes",
                project.default_delivery_mode
            )));
        }
        let read_write_count = project
            .repositories
            .values()
            .filter(|r| r.agent_access == RepositoryAccess::ReadWrite)
            .count();
        if project
            .allowed_delivery_modes
            .contains(&DeliveryMode::DirectPushFastForward)
            && read_write_count > 1
        {
            problems.push(problem(format!(
                "{ctx}: direct_push_fast_forward is limited to projects with one read_write repository"
            )));
        }
        if project.effects.publish_adaptation_merge_requests && read_write_count == 0 {
            problems.push(problem(format!(
                "{ctx}: publish_adaptation_merge_requests needs at least one read_write repository"
            )));
        }
        if project.limits.wall_clock_seconds == 0 || project.limits.max_commits_per_repository == 0
        {
            problems.push(problem(format!("{ctx}: limits must be positive")));
        }
        validate_network(&ctx, &project.network, &mut problems);
        if project.repositories.is_empty() {
            problems.push(problem(format!(
                "{ctx}: at least one repository is required"
            )));
        }
        let mut remotes = BTreeSet::new();
        for (repo_id, repo) in &project.repositories {
            let rctx = format!("{ctx}, repository {repo_id}");
            if !is_plain_name(repo_id) {
                problems.push(problem(format!("{rctx}: id is not a plain name")));
            }
            if !config.forges.contains_key(&repo.forge) {
                problems.push(problem(format!(
                    "{rctx}: forge {:?} does not exist",
                    repo.forge
                )));
            }
            if repo.remote.trim().is_empty() || repo.branch.trim().is_empty() {
                problems.push(problem(format!("{rctx}: remote and branch are mandatory")));
            }
            if !remotes.insert((&repo.forge, &repo.remote)) {
                problems.push(problem(format!(
                    "{rctx}: the same remote appears twice in the project"
                )));
            }
            if repo.description.trim().is_empty() {
                problems.push(problem(format!("{rctx}: description must not be empty")));
            }
            for step in repo.setup_recipe.iter().chain(&repo.verification_recipe) {
                if step.command.is_empty() {
                    problems.push(problem(format!(
                        "{rctx}: recipe step {} has an empty command",
                        step.name
                    )));
                }
                if step.timeout_seconds == 0 {
                    problems.push(problem(format!(
                        "{rctx}: recipe step {} needs a timeout",
                        step.name
                    )));
                }
                if step.cwd.starts_with('/') || step.cwd.split('/').any(|c| c == "..") {
                    problems.push(problem(format!(
                        "{rctx}: recipe step {} cwd must stay inside the repository",
                        step.name
                    )));
                }
            }
            if let Some(tagging) = &repo.tagging {
                if repo.agent_access == RepositoryAccess::ReadOnly {
                    problems.push(problem(format!(
                        "{rctx}: read_only repositories cannot carry tagging"
                    )));
                }
                if Regex::new(&tagging.allowed_pattern).is_err() {
                    problems.push(problem(format!(
                        "{rctx}: tagging.allowed_pattern is not a valid regex"
                    )));
                }
            }
            if let Some(paths) = &repo.allowed_paths {
                if paths.is_empty() {
                    problems.push(problem(format!(
                        "{rctx}: allowed_paths must be omitted or non-empty"
                    )));
                }
                if repo.agent_access == RepositoryAccess::ReadOnly {
                    problems.push(problem(format!(
                        "{rctx}: allowed_paths is meaningless for read_only"
                    )));
                }
            }
        }
    }

    problems
}

pub(crate) fn validate_network(ctx: &str, network: &NetworkPolicy, problems: &mut Vec<Problem>) {
    let egress = &network.command_egress;
    match (egress.mode, egress.allowed_domains.as_ref()) {
        (EgressMode::DomainAllowlist, None) => problems.push(problem(format!(
            "{ctx}: network.command_egress.allowed_domains is mandatory in domain_allowlist mode"
        ))),
        (EgressMode::DomainAllowlist, Some(domains)) if domains.is_empty() => {
            problems.push(problem(format!(
                "{ctx}: network.command_egress.allowed_domains must not be empty"
            )))
        }
        (EgressMode::DomainAllowlist, Some(_)) => {}
        (_, Some(_)) => problems.push(problem(format!(
            "{ctx}: network.command_egress.allowed_domains is only allowed in domain_allowlist mode"
        ))),
        (_, None) => {}
    }
    match (egress.private_network, egress.allowed_private_endpoints.is_empty()) {
        (PrivateNetworkMode::DomainAllowlist, true) => problems.push(problem(format!(
            "{ctx}: network.command_egress.allowed_private_endpoints is mandatory when private_network is domain_allowlist"
        ))),
        (PrivateNetworkMode::Blocked, false) => problems.push(problem(format!(
            "{ctx}: network.command_egress.allowed_private_endpoints must be empty when private_network is blocked"
        ))),
        _ => {}
    }
    let search = &network.web_search;
    match (search.domain_scope, search.allowed_domains.as_ref()) {
        (WebSearchScope::DomainAllowlist, None) => problems.push(problem(format!(
            "{ctx}: network.web_search.allowed_domains is mandatory in domain_allowlist scope"
        ))),
        (WebSearchScope::DomainAllowlist, Some(d)) if d.is_empty() => problems.push(problem(
            format!("{ctx}: network.web_search.allowed_domains must not be empty"),
        )),
        (WebSearchScope::PublicInternet, Some(_)) => problems.push(problem(format!(
            "{ctx}: network.web_search.allowed_domains is only allowed in domain_allowlist scope"
        ))),
        _ => {}
    }
}
