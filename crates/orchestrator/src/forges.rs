// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Builds forge adapters from configuration plus secrets.

use std::collections::BTreeMap;
use std::path::Path;

use artidezi_config::Config;
use artidezi_config::model::ForgeProvider;
use artidezi_forge::ForgeAdapter;
use artidezi_forge::fake::FakeForge;
use artidezi_forge::forgejo::ForgejoForge;
use artidezi_forge::github::{GithubAppIdentity, GithubForge};

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
                Box::new(ForgejoForge::new(
                    &forge.api_base_url,
                    &token,
                    &forge.bot_login,
                ))
            }
            ForgeProvider::Fake => Box::new(FakeForge::new(&fake_root.join(name))),
            ForgeProvider::Github => Box::new(github_binding(config, secrets, name)?),
        };
        forges.insert(name.clone(), adapter);
    }
    Ok(forges)
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
    let action_repositories = |merge: bool| {
        config
            .projects
            .values()
            .flat_map(|p| {
                p.pull_requests
                    .iter()
                    .filter(move |d| {
                        d.handlers.iter().any(|h| {
                            use artidezi_config::model::{PullRequestMode, SafeVerdictAction};
                            h.mode == PullRequestMode::Publish
                                && if merge {
                                    h.safe_verdict_action == SafeVerdictAction::Merge
                                } else {
                                    h.safe_verdict_action != SafeVerdictAction::Comment
                                }
                        })
                    })
                    .flat_map(move |d| {
                        d.repositories
                            .iter()
                            .filter_map(|id| p.repositories.get(id))
                    })
            })
            .filter(|r| r.forge == name)
            .map(|r| r.remote.clone())
            .collect()
    };
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
    .with_source_actions(action_repositories(false), action_repositories(true))?)
}

/// Live authentication preflight, independent of SQLite, the scheduler and the worker.
pub fn verify_github_binding(
    config: &Config,
    secrets: &SecretsDir,
    name: &str,
) -> Result<Vec<String>> {
    Ok(github_binding(config, secrets, name)?.verify_installation()?)
}
