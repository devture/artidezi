// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Builds forge adapters from configuration plus secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use artidezi_config::Config;
use artidezi_config::model::{
    ForgeProvider, PullRequestHandler, PullRequestMode, SafeVerdictAction, author_pattern,
};
use artidezi_forge::fake::FakeForge;
use artidezi_forge::forgejo::ForgejoForge;
use artidezi_forge::github::{GithubAppIdentity, GithubForge};
use artidezi_forge::{ForgeAdapter, MergeAuthor, SourceMergeAuthors};

use crate::secrets::SecretsDir;
use crate::{OrchestratorError, Result};

/// `fake_root`: where a `fake` forge keeps its bare repositories; only meaningful for local
/// experiments, so production configurations should not declare fake forges at all.
pub fn build_forges(
    config: &Config,
    secrets: &SecretsDir,
    fake_root: &Path,
) -> Result<BTreeMap<String, Box<dyn ForgeAdapter>>> {
    let mut forges: BTreeMap<String, Box<dyn ForgeAdapter>> = BTreeMap::new();
    for (name, forge) in &config.forges {
        let adapter: Box<dyn ForgeAdapter> = match forge.provider {
            ForgeProvider::Forgejo => {
                let token = secrets.get(&forge.credential_ref)?;
                Box::new(
                    ForgejoForge::new(&forge.api_base_url, &token, &forge.bot_login)
                        .with_source_actions(
                            source_action_repositories(config, name),
                            source_merge_authors(config, name),
                        ),
                )
            }
            ForgeProvider::Fake => Box::new(FakeForge::new(&fake_root.join(name))),
            ForgeProvider::Github => Box::new(github_binding(config, secrets, name)?),
        };
        forges.insert(name.clone(), adapter);
    }
    Ok(forges)
}

/// Remotes on forge `name` watched by a publish handler that may approve unchanged sources.
fn source_action_repositories(config: &Config, name: &str) -> BTreeSet<String> {
    source_action_handlers(config, name)
        .filter(|(_, handler)| handler.safe_verdict_action != SafeVerdictAction::Comment)
        .map(|(remote, _)| remote)
        .collect()
}

/// Authors per remote on forge `name` whose unchanged sources a merge handler may merge.
fn source_merge_authors(config: &Config, name: &str) -> SourceMergeAuthors {
    let mut authors = SourceMergeAuthors::default();
    for (remote, handler) in source_action_handlers(config, name)
        .filter(|(_, handler)| handler.safe_verdict_action == SafeVerdictAction::Merge)
    {
        if handler.catch_all {
            authors.allow(&remote, MergeAuthor::Any);
        }
        for login in handler.authors.iter().flatten() {
            authors.allow(&remote, MergeAuthor::Login(login.clone()));
        }
        // Validation rejects an invalid pattern; one that fails here grants nothing.
        if let Some(pattern) = handler
            .author_regex
            .as_deref()
            .and_then(|p| author_pattern(p).ok())
        {
            authors.allow(&remote, MergeAuthor::Pattern(pattern));
        }
    }
    authors
}

/// Publish handlers paired with each watched remote on forge `name`.
fn source_action_handlers<'a>(
    config: &'a Config,
    name: &str,
) -> impl Iterator<Item = (String, &'a PullRequestHandler)> {
    let mut pairs = Vec::new();
    for project in config.projects.values() {
        let Some(discovery) = &project.pull_requests else {
            continue;
        };
        for id in &discovery.repositories {
            let Some(repository) = project.repositories.get(id).filter(|r| r.forge == name) else {
                continue;
            };
            for handler in &discovery.handlers {
                if handler.mode == PullRequestMode::Publish {
                    pairs.push((repository.remote.clone(), handler));
                }
            }
        }
    }
    pairs.into_iter()
}

fn github_binding(config: &Config, secrets: &SecretsDir, name: &str) -> Result<GithubForge> {
    let forge = config
        .forges
        .get(name)
        .filter(|f| f.provider == ForgeProvider::Github)
        .ok_or_else(|| {
            OrchestratorError::Invalid(format!("forge {name}: not a configured GitHub binding"))
        })?;
    let app = forge.github_app.as_ref().ok_or_else(|| {
        OrchestratorError::Invalid(format!("forge {name}: github_app is required"))
    })?;
    let repositories = config
        .projects
        .values()
        .flat_map(|p| p.repositories.values())
        .filter(|r| r.forge == name)
        .map(|r| r.remote.clone())
        .collect();
    let publication_repositories = config
        .projects
        .values()
        .filter(|p| {
            p.effects.publish_adaptation_merge_requests
                && (p.effects.allow_manual_github_publication
                    || p.pull_requests.as_ref().is_some_and(|d| {
                        d.handlers.iter().any(|h| {
                            h.mode == artidezi_config::model::PullRequestMode::Publish
                                && h.action == artidezi_config::model::PullRequestAction::Assess
                        })
                    }))
        })
        .flat_map(|p| p.repositories.values())
        .filter(|r| {
            r.forge == name
                && r.agent_access == artidezi_contract::common::RepositoryAccess::ReadWrite
        })
        .map(|r| r.remote.clone())
        .collect();
    Ok(GithubForge::new(
        GithubAppIdentity {
            client_id: app.client_id.clone(),
            installation_id: app.installation_id,
            account: app.account.clone(),
            bot_login: forge.bot_login.clone(),
        },
        &secrets.get(&forge.credential_ref)?,
        repositories,
    )?
    .with_publication_repositories(publication_repositories)?
    .with_source_actions(
        source_action_repositories(config, name),
        source_merge_authors(config, name),
    )?)
}

/// Live authentication preflight, independent of SQLite, the scheduler and the worker.
pub fn verify_github_binding(
    config: &Config,
    secrets: &SecretsDir,
    name: &str,
) -> Result<Vec<String>> {
    Ok(github_binding(config, secrets, name)?.verify_installation()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_handlers(handlers: &str) -> Config {
        let yaml = include_str!("../../config/tests/fixtures/valid.yaml").replace(
            "      merge_source_merge_request_on_safe_verdict: false",
            "      merge_source_merge_request_on_safe_verdict: true",
        );
        let config = artidezi_config::parse(&format!(
            "{yaml}    pull_requests:\n      poll_interval_seconds: 60\n      settle_seconds: 0\n      max_runs_per_pull_request: 1\n      max_active_tasks: 1\n      repositories: [ansible-role-notes]\n      handlers:\n{handlers}"
        ))
        .unwrap();
        assert!(
            artidezi_config::validate(&config).is_empty(),
            "{:?}",
            artidezi_config::validate(&config)
        );
        config
    }

    fn handler(id: &str, selector: &str, action: &str) -> String {
        format!(
            "        - id: {id}\n          {selector}\n          mode: publish\n          safe_verdict_action: {action}\n          action: assess\n          require_successful_checks: true\n          instructions: Assess.\n"
        )
    }

    #[test]
    fn merge_scope_follows_each_merge_handlers_author_selector() {
        let remote = "acme/ansible-role-notes";
        let config = with_handlers(&format!(
            "{}{}",
            handler("bots", r#"author_regex: '[a-z-]+\[bot\]'"#, "merge"),
            handler("people", "authors: [maintainer]", "approve"),
        ));
        let authors = source_merge_authors(&config, "forge-acme");
        assert!(authors.permits(remote, "Renovate[bot]"));
        assert!(!authors.permits(remote, "renovate[bot]-x"));
        // An approve handler grants no merge.
        assert!(!authors.permits(remote, "maintainer"));
        assert!(source_action_repositories(&config, "forge-acme").contains(remote));

        let config = with_handlers(&handler("everyone", "catch_all: true", "merge"));
        let authors = source_merge_authors(&config, "forge-acme");
        assert!(authors.permits(remote, "anyone"));
        assert!(!authors.permits("acme/acme-playbook", "anyone"));
    }
}
