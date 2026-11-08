// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The trusted orchestrator core, independent of any transport. An HTTP API and UI will wrap it;
//! the offline test suite drives it directly together with the in-process worker.

pub mod discovery;
pub mod forges;
pub mod ingest;
pub mod maintenance;
pub mod prompt;
pub mod publish;
mod recovery;
pub mod secrets;
mod source_ci;
mod source_publish;
mod source_supersession;
pub mod state;
pub mod tasks;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use artidezi_config::Config;
use artidezi_forge::ForgeAdapter;
use artidezi_git::Identity;
use artidezi_store::{ArtifactStore, Db};

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Store(#[from] artidezi_store::StoreError),
    #[error(transparent)]
    Git(#[from] artidezi_git::GitError),
    #[error("libgit2 error: {0}")]
    Git2(#[from] git2::Error),
    #[error(transparent)]
    Forge(#[from] artidezi_forge::ForgeError),
    #[error(transparent)]
    Resolve(#[from] artidezi_config::resolve::ResolveError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("source head changed from {expected} to {actual}; a fresh evaluation is required")]
    SourceHeadChanged {
        expected: artidezi_contract::common::CommitSha,
        actual: artidezi_contract::common::CommitSha,
    },
    #[error("Approval/merge is waiting for successful current forge checks")]
    WaitingForCi,
}

pub type Result<T> = std::result::Result<T, OrchestratorError>;

/// The database copy that file backups rely on, relative to the data directory. Never opened by
/// the orchestrator; restoring means stopping it and moving this file over `artidezi.sqlite`.
pub const SNAPSHOT_PATH: &str = "backup/artidezi.sqlite";
const SNAPSHOT_INTERVAL_SECONDS: i64 = 3600;

pub struct Orchestrator {
    pub config: Config,
    pub db: Db,
    pub artifacts: ArtifactStore,
    caches_root: PathBuf,
    forges: BTreeMap<String, Box<dyn ForgeAdapter>>,
    bot: Identity,
    last_artifact_cleanup: chrono::DateTime<chrono::Utc>,
    snapshot_path: PathBuf,
    last_snapshot: Option<chrono::DateTime<chrono::Utc>>,
}

impl Orchestrator {
    /// Opens (or creates) the instance's durable state under `data_dir` and wires the given forge
    /// adapters, which must cover every forge named in the configuration.
    pub fn open(
        config: Config,
        data_dir: &Path,
        forges: BTreeMap<String, Box<dyn ForgeAdapter>>,
    ) -> Result<Self> {
        for name in config.forges.keys() {
            if !forges.contains_key(name) {
                return Err(OrchestratorError::Invalid(format!(
                    "no adapter supplied for forge {name}"
                )));
            }
        }
        std::fs::create_dir_all(data_dir).map_err(|source| OrchestratorError::Io {
            path: data_dir.to_owned(),
            source,
        })?;
        // Holds password hashes, private snapshots and logs.
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| OrchestratorError::Io {
                path: data_dir.to_owned(),
                source,
            },
        )?;
        let db = Db::open(&data_dir.join("artidezi.sqlite"))?;
        let listed: Vec<(&str, &str)> = config
            .worker_pools
            .iter()
            .flat_map(|(pool, definition)| {
                definition
                    .workers
                    .keys()
                    .map(move |id| (pool.as_str(), id.as_str()))
            })
            .collect();
        db.forget_unlisted_workers(&listed)?;
        let artifacts = ArtifactStore::open(data_dir.join("artifacts"))?;
        let bot = Identity {
            name: config.instance.bot_identity.name.clone(),
            email: config.instance.bot_identity.email.clone(),
        };
        let mut orchestrator = Self {
            config,
            db,
            artifacts,
            caches_root: data_dir.join("caches"),
            forges,
            bot,
            last_artifact_cleanup: chrono::Utc::now(),
            snapshot_path: data_dir.join(SNAPSHOT_PATH),
            last_snapshot: None,
        };
        // A crash can occur after accepting an upload but before finishing remote publication.
        // Startup only makes it actionable; it never retries a remote effect automatically.
        use artidezi_store::model::{AttemptState, TaskKind, TaskState};
        for task in orchestrator.db.list_tasks(Some(TaskState::Working))? {
            let delivers = task.kind == TaskKind::Manual
                || (task.kind == TaskKind::PullRequest
                    && task.delivery_mode == artidezi_config::model::DeliveryMode::MergeRequest);
            if delivers
                && let Some(attempt) = orchestrator.db.latest_attempt(&task.id)?
                && attempt.state == AttemptState::Valid
            {
                orchestrator.needs_human(
                    &task,
                    &attempt,
                    "Publication interrupted by restart; retry delivery using the validated result",
                )?;
            }
        }
        Ok(orchestrator)
    }

    pub fn forge(&self, name: &str) -> Result<&dyn ForgeAdapter> {
        if let Some(retry_at) = self.db.forge_retry_after(name)?
            && retry_at > chrono::Utc::now()
        {
            return Err(artidezi_forge::ForgeError::RetryAfter { retry_at }.into());
        }
        self.forges
            .get(name)
            .map(|f| f.as_ref())
            .ok_or_else(|| OrchestratorError::Invalid(format!("unknown forge {name}")))
    }

    pub(crate) fn persist_forge_backoffs(&self) -> Result<()> {
        for (name, adapter) in &self.forges {
            if let Some(retry_at) = adapter.retry_after() {
                self.db.defer_forge(name, retry_at)?;
            }
        }
        Ok(())
    }

    /// Bare cache for a remote. Rebuildable; the forge stays authoritative.
    pub fn cache(&self, forge: &str, remote: &str) -> Result<artidezi_git::cache::BareCache> {
        open_cache(&self.caches_root, forge, remote)
    }
}

pub(crate) fn open_cache(
    caches_root: &Path,
    forge: &str,
    remote: &str,
) -> Result<artidezi_git::cache::BareCache> {
    let safe: String = remote
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(artidezi_git::cache::BareCache::open_or_init(
        &caches_root.join(forge).join(format!("{safe}.git")),
    )?)
}
