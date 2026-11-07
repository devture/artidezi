// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Reachability-based expiry. Call under the controller's operation lock, using its shared store.
//! Jobs remain immutable: only their explicitly identified frozen repository inputs can expire.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use artidezi_config::model::ArtifactRetention;
use artidezi_contract::Job;
use artidezi_contract::common::ArtifactRef;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::model::MergeRequestState;
use crate::{ArtifactStore, Db, Result, StoreError};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub eligible_tasks: Vec<String>,
    pub retained_bytes: u64,
    pub reclaimable_bytes: u64,
    pub removed_bytes: u64,
    pub expired_bundle_files: usize,
    pub orphan_files: usize,
    pub publication_refresh_errors: Vec<String>,
    pub candidates: Vec<CleanupCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CleanupCandidate {
    pub artifact: ArtifactRef,
    pub reason: String,
}

fn io(path: &std::path::Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Recognizes only well-formed artifact references, including nested source-CI and audit data.
fn references(value: &Value, refs: &mut BTreeMap<String, ArtifactRef>) -> Result<()> {
    match value {
        Value::Object(object) => {
            if let (Some(sha256), Some(size)) = (
                object.get("sha256").and_then(Value::as_str),
                object.get("size").and_then(Value::as_u64),
            ) {
                let artifact = ArtifactRef {
                    sha256: sha256.to_owned(),
                    size,
                };
                if artifact.is_well_formed() {
                    if let Some(previous) = refs.insert(sha256.to_owned(), artifact)
                        && previous.size != size
                    {
                        return Err(StoreError::CorruptArtifact {
                            sha256: sha256.to_owned(),
                            reason: "conflicting reference sizes".into(),
                        });
                    }
                    return Ok(());
                }
            }
            for child in object.values() {
                references(child, refs)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                references(child, refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Plans and optionally applies expiry under a barrier shared with authenticated worker uploads.
/// A missing/corrupt retained document or missing protected artifact aborts before any deletion.
/// Multiple controllers/offline callers against the same directory are unsupported.
pub fn cleanup(
    db: &mut Db,
    store: &ArtifactStore,
    policy: &ArtifactRetention,
    now: DateTime<Utc>,
    apply: bool,
) -> Result<CleanupReport> {
    let _guard = store.cleanup_guard();
    let completed_before =
        now - Duration::seconds(i64::from(policy.completed_task_bundle_retention_seconds));
    let orphan_before = now - Duration::seconds(i64::from(policy.orphan_grace_seconds));
    let mut report = CleanupReport {
        dry_run: !apply,
        ..CleanupReport::default()
    };
    let mut roots = BTreeMap::new();
    let mut documents = BTreeMap::new();
    let mut jobs: BTreeMap<String, (Job, bool)> = BTreeMap::new();
    let mut protected_attempts = BTreeSet::new();
    let mut expirable = BTreeMap::new();
    for task in db.list_tasks(None)? {
        let attempts = db.list_attempts(&task.id)?;
        let eligible = task.state.is_terminal()
            && (task.updated_at <= completed_before || db.task_inputs_expired(&task.id)?)
            && attempts.iter().all(|a| a.state.is_terminal())
            && db
                .list_merge_requests(&task.id)?
                .iter()
                .all(|mr| mr.state != MergeRequestState::Open);
        if eligible {
            report.eligible_tasks.push(task.id.clone());
        }
        let mut frozen_policy: Value = serde_json::from_str(&task.policy_snapshot)?;
        if eligible
            && let Some(input) = frozen_policy
                .pointer_mut("/source_pull_request/input")
                .and_then(Value::as_object_mut)
            && let Some(bundle) = input.remove("bundle")
        {
            // Discovery records the same immutable PR input in both the job and policy.
            // Keep source identity and CI evidence; expire this explicit duplicate bundle edge.
            let artifact: ArtifactRef = serde_json::from_value(bundle)?;
            if !artifact.is_well_formed() {
                return Err(StoreError::MissingArtifact(artifact.sha256));
            }
            expirable.insert(artifact.sha256.clone(), artifact);
        }
        references(&frozen_policy, &mut roots)?;
        for attempt in attempts {
            if !eligible {
                protected_attempts.insert(attempt.id.clone());
            }
            let job: Job = store.get_json(&attempt.job)?;
            jobs.entry(attempt.job.sha256.clone())
                .and_modify(|(_, expire)| *expire &= eligible)
                .or_insert((job, eligible));
            roots.insert(attempt.job.sha256.clone(), attempt.job);
            if let Some(artifact) = attempt.attempt_record {
                let value: Value = store.get_json(&artifact)?;
                documents.insert(artifact.sha256.clone(), value);
                roots.insert(artifact.sha256.clone(), artifact);
            }
            if let Some(artifact) = attempt.result {
                // Malformed model JSON remains useful failure evidence. AgentResult has no
                // artifact-reference fields, so its raw bytes never define reachability.
                store.get(&artifact)?;
                roots.insert(artifact.sha256.clone(), artifact);
            }
        }
    }
    for (sha256, (job, expire)) in jobs {
        let mut value = serde_json::to_value(&job)?;
        if expire {
            for repo in &job.repositories {
                expirable.insert(
                    repo.snapshot_bundle.sha256.clone(),
                    repo.snapshot_bundle.clone(),
                );
                for input in &repo.input_refs {
                    expirable.insert(input.bundle.sha256.clone(), input.bundle.clone());
                }
            }
            // Drop only these contract fields. Identical bytes used as a log, result, previous
            // output series or another task's live input are still reachable and stay protected.
            if let Some(repositories) = value.get_mut("repositories").and_then(Value::as_array_mut)
            {
                for repo in repositories {
                    if let Some(object) = repo.as_object_mut() {
                        object.remove("snapshot_bundle");
                    }
                    if let Some(inputs) = repo.get_mut("input_refs").and_then(Value::as_array_mut) {
                        for input in inputs {
                            if let Some(object) = input.as_object_mut() {
                                object.remove("bundle");
                            }
                        }
                    }
                }
            }
        }
        documents.insert(sha256, value);
    }
    for value in db.retention_metadata()? {
        references(&value, &mut roots)?;
    }
    let mut recent_uploads = BTreeSet::new();
    for (attempt_id, artifact, uploaded_at) in db.artifact_uploads()? {
        if uploaded_at > orphan_before {
            recent_uploads.insert(artifact.sha256.clone());
        }
        // A pin can precede its file if a process crashes during the upload. Absence does not
        // make it a corrupt history reference, but any existing file must remain protected.
        if protected_attempts.contains(&attempt_id) && store.contains(&artifact) {
            roots.insert(artifact.sha256.clone(), artifact);
        }
    }
    let mut visited = BTreeSet::new();
    loop {
        let pending: Vec<_> = roots
            .keys()
            .filter(|sha| !visited.contains(*sha))
            .cloned()
            .collect();
        if pending.is_empty() {
            break;
        }
        for sha in pending {
            visited.insert(sha.clone());
            if let Some(document) = documents.get(&sha) {
                references(document, &mut roots)?;
            }
        }
    }
    // Preflight all retained references, including previously uploaded unpublished series.
    for artifact in roots.values() {
        let path = store.path(artifact)?;
        let metadata = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
        if !metadata.file_type().is_file() || metadata.len() != artifact.size {
            return Err(StoreError::CorruptArtifact {
                sha256: artifact.sha256.clone(),
                reason: "retained artifact is not a regular file of the expected size".into(),
            });
        }
    }
    for artifact in expirable.values() {
        if !store.contains(artifact) && !db.artifact_was_deleted(&artifact.sha256)? {
            return Err(StoreError::MissingArtifact(artifact.sha256.clone()));
        }
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(store.root()).map_err(|e| io(store.root(), e))? {
        let entry = entry.map_err(|e| io(store.root(), e))?;
        let prefix = entry.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            continue;
        }
        let dir = entry.path();
        if !entry.file_type().map_err(|e| io(&dir, e))?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
            let entry = entry.map_err(|e| io(&dir, e))?;
            let path = entry.path();
            let sha256 = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
            let artifact = ArtifactRef {
                sha256: sha256.clone(),
                size: metadata.len(),
            };
            if !artifact.is_well_formed()
                || !sha256.starts_with(&prefix)
                || !metadata.file_type().is_file()
            {
                continue;
            }
            let expired = expirable.contains_key(&sha256);
            let modified: DateTime<Utc> = metadata.modified().map_err(|e| io(&path, e))?.into();
            if roots.contains_key(&sha256)
                || recent_uploads.contains(&sha256)
                || (!expired && modified > orphan_before)
            {
                report.retained_bytes += metadata.len();
                continue;
            }
            if expired {
                if expirable[&sha256].size != artifact.size {
                    return Err(StoreError::CorruptArtifact {
                        sha256,
                        reason: "expiring input size mismatch".into(),
                    });
                }
                report.expired_bundle_files += 1;
            } else {
                report.orphan_files += 1;
            }
            report.reclaimable_bytes += artifact.size;
            report.candidates.push(CleanupCandidate {
                artifact,
                reason: if expired {
                    "completed_task_repository_input"
                } else {
                    "unreferenced"
                }
                .into(),
            });
            paths.push(path);
        }
    }
    if apply {
        for task_id in &report.eligible_tasks {
            db.expire_task_inputs(task_id, now)?;
        }
        for (candidate, path) in report.candidates.iter().zip(paths) {
            // The receipt precedes unlinking so retry recognizes intentionally missing inputs.
            db.record_artifact_deletion(&candidate.artifact, now, &candidate.reason)?;
            fs::remove_file(&path).map_err(|e| io(&path, e))?;
            report.removed_bytes += candidate.artifact.size;
        }
        db.record_event(None, None, "orchestrator", "artifacts.cleaned", serde_json::json!({
            "removed_bytes": report.removed_bytes, "expired_bundle_files": report.expired_bundle_files,
            "orphan_files": report.orphan_files, "eligible_tasks": report.eligible_tasks,
        }))?;
    }
    if report.removed_bytes > 0 {
        tracing::info!(
            removed_bytes = report.removed_bytes,
            expired_bundle_files = report.expired_bundle_files,
            orphan_files = report.orphan_files,
            "artifact retention cleanup"
        );
    }
    Ok(report)
}
