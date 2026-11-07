// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Local state access without the serialized Git/forge operation lock.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use artidezi_config::Config;
use artidezi_contract::Job;
use artidezi_contract::wire::AdvertisedHarness;
use artidezi_store::model::{Attempt, AttemptState, Lease};
use artidezi_store::{ArtifactStore, Db, new_id};
use chrono::{DateTime, Duration, Utc};

use crate::ingest::{LeasedWork, PreviousFiles};
use crate::{Orchestrator, OrchestratorError, Result};

/// Database operations acquire only the short connection lock. Artifact I/O is independent.
/// This handle has no forge adapters, so it can't publish changes. Its Git cache access is limited
/// to inspecting worker bundles.
#[derive(Clone)]
pub struct StateAccess {
    pub config: Arc<Config>,
    pub db: Db,
    pub artifacts: ArtifactStore,
    pub(crate) caches_root: PathBuf,
}

impl Orchestrator {
    /// Server configuration is fixed for the lifetime of a running instance.
    pub fn state_access(&self) -> StateAccess {
        StateAccess {
            config: Arc::new(self.config.clone()),
            db: self.db.clone(),
            artifacts: self.artifacts.clone(),
            caches_root: self.caches_root.clone(),
        }
    }
}

impl StateAccess {
    /// Local-only recovery must not wait for the forge/preparation operation gate.
    pub fn recover_expired_at(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let lost = self.db.expire_leases(now)?;
        for attempt in self.db.pending_lost_attempts()? {
            let max_retries = self
                .config
                .worker_pools
                .get(&attempt.worker_pool)
                .map_or(0, |pool| pool.lost_attempt_retries);
            if max_retries == 0 {
                self.db.recover_lost_attempt(&attempt.id, None, 0)?;
                continue;
            }
            let replacement = self.replacement_attempt(&attempt, now);
            match replacement {
                Ok(next) => {
                    self.db
                        .recover_lost_attempt(&attempt.id, Some(&next), max_retries)?;
                }
                Err(error) => {
                    tracing::error!(attempt = %attempt.id, %error, "could not prepare lost execution recovery");
                    self.db
                        .recover_lost_attempt(&attempt.id, None, max_retries)?;
                }
            }
        }
        for attempt in self.db.pending_transient_failures()? {
            let max_retries = self
                .config
                .worker_pools
                .get(&attempt.worker_pool)
                .map_or(0, |pool| pool.transient_failure_retries);
            let next = self.replacement_attempt(&attempt, now)?;
            self.db
                .recover_transient_attempt(&attempt.id, &next, max_retries, now)?;
        }
        Ok(lost)
    }

    fn replacement_attempt(&self, attempt: &Attempt, now: DateTime<Utc>) -> Result<Attempt> {
        let mut job = self.load_job(attempt)?;
        let id = new_id("att");
        job.schema_version = artidezi_contract::job::JOB_SCHEMA_VERSION;
        job.attempt_id = id.as_str().into();
        job.attempt_number = attempt
            .number
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::Invalid("attempt number exhausted".into()))?;
        // Retain every frozen input, including the exact original prompt and revision
        // context. This is a fresh execution, not a new human instruction or source run.
        let artifact = self.artifacts.put_json(&job)?;
        Ok(Attempt {
            id,
            task_id: attempt.task_id.clone(),
            number: job.attempt_number,
            state: AttemptState::Queued,
            created_at: now,
            updated_at: now,
            worker_pool: attempt.worker_pool.clone(),
            job: artifact,
            human_feedback: attempt.human_feedback.clone(),
            not_before: None,
            attempt_record: None,
            result: None,
            outcome: None,
            validation_problems: vec![],
        })
    }

    pub fn load_job(&self, attempt: &Attempt) -> Result<Job> {
        Ok(self.artifacts.get_json(&attempt.job)?)
    }

    /// Checks expiry even before the maintenance timer has marked an attempt lost.
    pub fn require_live_lease(&self, attempt: &Attempt, now: DateTime<Utc>) -> Result<()> {
        let lease = self.db.get_lease(&attempt.id)?;
        let job = self.load_job(attempt)?;
        if !lease.is_some_and(|lease| {
            lease.expires_at > now && execution_deadline(&job, lease.leased_at) > now
        }) {
            return Err(artidezi_store::StoreError::NotFound {
                entity: "lease",
                id: attempt.id.clone(),
            }
            .into());
        }
        Ok(())
    }

    pub fn lease(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
    ) -> Result<Option<LeasedWork>> {
        let leased = self
            .db
            .lease_next_attempt(worker_pool, worker_id, ttl, Utc::now())?;
        self.prepare_lease(leased, worker_id, ttl)
    }

    /// Leases the oldest attempt this worker can run. Others stay queued for a capable worker.
    pub fn lease_compatible(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
        harnesses: &[AdvertisedHarness],
        tags: &[String],
    ) -> Result<Option<LeasedWork>> {
        let artifacts = self.artifacts.clone();
        let leased = self.db.lease_next_attempt_matching(
            worker_pool,
            worker_id,
            ttl,
            Utc::now(),
            |attempt| {
                artifacts.get_json::<Job>(&attempt.job).is_ok_and(|job| {
                    job.agent_profile.runs_on(harnesses) && job.worker_requirements.met_by(tags)
                })
            },
        )?;
        self.prepare_lease(leased, worker_id, ttl)
    }

    fn prepare_lease(
        &mut self,
        leased: Option<(Attempt, Lease)>,
        worker_id: &str,
        ttl: Duration,
    ) -> Result<Option<LeasedWork>> {
        let Some((attempt, lease)) = leased else {
            return Ok(None);
        };
        let job_bytes = self.artifacts.get(&attempt.job)?;
        let job: Job = serde_json::from_slice(&job_bytes)
            .map_err(|e| OrchestratorError::Invalid(e.to_string()))?;
        self.db.heartbeat_until(
            &attempt.id,
            worker_id,
            ttl,
            Utc::now(),
            execution_deadline(&job, lease.leased_at),
        )?;
        let task_document = self.artifacts.get(&job.task_document)?;
        let mut snapshot_bundles = BTreeMap::new();
        let mut input_ref_bundles = BTreeMap::new();
        for repo in &job.repositories {
            snapshot_bundles.insert(repo.id.clone(), self.artifacts.path(&repo.snapshot_bundle)?);
            for input in &repo.input_refs {
                input_ref_bundles.insert(
                    (repo.id.clone(), input.name.clone()),
                    self.artifacts.path(&input.bundle)?,
                );
            }
        }
        let previous = match &job.previous_attempt {
            Some(prev) => {
                let mut commit_bundles = BTreeMap::new();
                for (repo_id, bundle) in &prev.commit_bundles {
                    commit_bundles.insert(repo_id.clone(), self.artifacts.path(bundle)?);
                }
                Some(PreviousFiles {
                    attempt_document: self.artifacts.get(&prev.attempt_document)?,
                    result_document: prev
                        .result_document
                        .as_ref()
                        .map(|r| self.artifacts.get(r))
                        .transpose()?,
                    commit_bundles,
                })
            }
            None => None,
        };
        Ok(Some(LeasedWork {
            attempt,
            job,
            job_bytes,
            task_document,
            snapshot_bundles,
            input_ref_bundles,
            previous,
        }))
    }

    pub fn heartbeat(&mut self, attempt_id: &str, worker_id: &str, ttl: Duration) -> Result<()> {
        self.heartbeat_at(attempt_id, worker_id, ttl, Utc::now())
    }

    /// Uses controller time and frozen job limits, never a worker-supplied extension budget.
    pub fn heartbeat_at(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let attempt = self.db.get_attempt(attempt_id)?;
        let job = self.load_job(&attempt)?;
        let lease =
            self.db
                .get_lease(attempt_id)?
                .ok_or_else(|| artidezi_store::StoreError::NotFound {
                    entity: "lease",
                    id: attempt_id.to_owned(),
                })?;
        self.db.heartbeat_until(
            attempt_id,
            worker_id,
            ttl,
            now,
            execution_deadline(&job, lease.leased_at),
        )?;
        Ok(())
    }
}

// Harness wall time excludes setup and verification. Allow every configured recipe's full
// budget and termination grace, plus 30 minutes for transfer/preparation/collection overhead.
// This is an independent controller ceiling, including for older or unhealthy workers.
fn execution_deadline(job: &Job, leased_at: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = job
        .repositories
        .iter()
        .flat_map(|repo| repo.setup_recipe.iter().chain(&repo.verification_recipe))
        .fold(
            job.limits.wall_clock_seconds.saturating_add(1800 + 15),
            |budget, step| {
                budget
                    .saturating_add(step.timeout_seconds)
                    .saturating_add(15)
            },
        );
    let duration = Duration::try_seconds(i64::try_from(seconds).unwrap_or(i64::MAX));
    duration
        .and_then(|duration| leased_at.checked_add_signed(duration))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}
