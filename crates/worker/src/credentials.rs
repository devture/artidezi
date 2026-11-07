// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Durable handling for harness credentials that rotate during an attempt.

use std::fs;
use std::io::Write;

use artidezi_harness::{CredentialFileKind, CredentialFileUpdate};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Result, WorkerError, io_err};

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexChatgptIdentity {
    auth_mode: String,
    account_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedCredentialUpdate {
    update: CredentialFileUpdate,
    source_digest: [u8; 32],
    identity: CodexChatgptIdentity,
}

pub(crate) fn prepare(updates: &[CredentialFileUpdate]) -> Result<Vec<PreparedCredentialUpdate>> {
    updates
        .iter()
        .map(|update| {
            require_regular_file(&update.source, "durable credential")?;
            require_regular_file(&update.attempt, "attempt credential")?;
            let source = fs::read(&update.source).map_err(io_err(&update.source))?;
            let attempt = fs::read(&update.attempt).map_err(io_err(&update.attempt))?;
            if source != attempt {
                return Err(WorkerError::Invalid(format!(
                    "credential {}: attempt copy differs before the harness starts",
                    update.credential_ref
                )));
            }
            let identity = validate(update.kind, &source).map_err(|reason| {
                WorkerError::Invalid(format!("credential {}: {reason}", update.credential_ref))
            })?;
            Ok(PreparedCredentialUpdate {
                update: update.clone(),
                source_digest: Sha256::digest(&source).into(),
                identity,
            })
        })
        .collect()
}

/// Persists every valid rotation independently and returns sanitized warnings for failures.
pub(crate) fn persist(updates: &[PreparedCredentialUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|prepared| {
            persist_one(prepared).err().map(|error| {
                format!(
                    "credential {}: rotated login was not persisted: {error}",
                    prepared.update.credential_ref
                )
            })
        })
        .collect()
}

fn persist_one(prepared: &PreparedCredentialUpdate) -> Result<()> {
    require_regular_file(&prepared.update.attempt, "updated attempt credential")?;
    let attempt = fs::read(&prepared.update.attempt).map_err(io_err(&prepared.update.attempt))?;
    if <[u8; 32]>::from(Sha256::digest(&attempt)) == prepared.source_digest {
        return Ok(());
    }

    let identity = validate(prepared.update.kind, &attempt).map_err(|reason| {
        WorkerError::Invalid(format!("updated credential is invalid: {reason}"))
    })?;
    if identity != prepared.identity {
        return Err(WorkerError::Invalid(
            "updated credential changed the authentication mode or account identity".into(),
        ));
    }

    let metadata = require_regular_file(&prepared.update.source, "durable credential")?;
    let current = fs::read(&prepared.update.source).map_err(io_err(&prepared.update.source))?;
    if <[u8; 32]>::from(Sha256::digest(&current)) != prepared.source_digest {
        return Err(WorkerError::Invalid(
            "durable credential changed concurrently; refusing to overwrite it".into(),
        ));
    }

    let parent =
        prepared.update.source.parent().ok_or_else(|| {
            WorkerError::Invalid("durable credential has no parent directory".into())
        })?;
    let permissions = metadata.permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_err(parent))?;
    temporary
        .as_file_mut()
        .write_all(&attempt)
        .map_err(io_err(temporary.path()))?;
    temporary
        .as_file_mut()
        .flush()
        .map_err(io_err(temporary.path()))?;
    fs::set_permissions(temporary.path(), permissions).map_err(io_err(temporary.path()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(io_err(temporary.path()))?;
    temporary
        .persist(&prepared.update.source)
        .map_err(|error| WorkerError::Io {
            path: prepared.update.source.clone(),
            source: error.error,
        })?;
    Ok(())
}

fn require_regular_file(path: &std::path::Path, label: &str) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(io_err(path))?;
    if !metadata.file_type().is_file() {
        return Err(WorkerError::Invalid(format!(
            "{label} is not a regular file"
        )));
    }
    Ok(metadata)
}

fn validate(
    kind: CredentialFileKind,
    bytes: &[u8],
) -> std::result::Result<CodexChatgptIdentity, String> {
    match kind {
        CredentialFileKind::CodexChatgptAuth => validate_codex_chatgpt_auth(bytes),
    }
}

fn validate_codex_chatgpt_auth(bytes: &[u8]) -> std::result::Result<CodexChatgptIdentity, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Codex auth.json does not parse: {error}"))?;
    let required = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("Codex auth.json lacks {pointer}"))
    };
    let auth_mode = required("/auth_mode")?;
    if auth_mode != "chatgpt" {
        return Err(format!(
            "Codex auth.json uses {auth_mode:?}, expected chatgpt"
        ));
    }
    let account_id = required("/tokens/account_id")?;
    required("/tokens/access_token")?;
    required("/tokens/id_token")?;
    required("/tokens/refresh_token")?;
    Ok(CodexChatgptIdentity {
        auth_mode,
        account_id,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn auth(account: &str, suffix: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{suffix}"),
                "account_id": account,
                "id_token": format!("id-{suffix}"),
                "refresh_token": format!("refresh-{suffix}")
            },
            "last_refresh": "2026-09-23T00:00:00Z"
        }))
        .unwrap()
    }

    fn fixture() -> (tempfile::TempDir, CredentialFileUpdate) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("credential/auth.json");
        let attempt = dir.path().join("attempt/.codex/auth.json");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(attempt.parent().unwrap()).unwrap();
        fs::write(&source, auth("account-1", "old")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        fs::copy(&source, &attempt).unwrap();
        (
            dir,
            CredentialFileUpdate {
                credential_ref: "personal-codex".into(),
                source,
                attempt,
                kind: CredentialFileKind::CodexChatgptAuth,
            },
        )
    }

    #[test]
    fn persists_valid_rotation_atomically_with_original_permissions() {
        let (_dir, update) = fixture();
        let prepared = prepare(std::slice::from_ref(&update)).unwrap();
        let rotated = auth("account-1", "new");
        fs::write(&update.attempt, &rotated).unwrap();

        assert!(persist(&prepared).is_empty());
        assert_eq!(fs::read(&update.source).unwrap(), rotated);
        assert_eq!(
            fs::metadata(&update.source).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn leaves_an_unchanged_credential_alone() {
        let (_dir, update) = fixture();
        let prepared = prepare(std::slice::from_ref(&update)).unwrap();

        assert!(persist(&prepared).is_empty());
        assert_eq!(fs::read(&update.source).unwrap(), auth("account-1", "old"));
    }

    #[test]
    fn refuses_concurrent_source_changes() {
        let (_dir, update) = fixture();
        let prepared = prepare(std::slice::from_ref(&update)).unwrap();
        let external = auth("account-1", "external");
        fs::write(&update.source, &external).unwrap();
        fs::write(&update.attempt, auth("account-1", "attempt")).unwrap();

        let warnings = persist(&prepared);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("changed concurrently"));
        assert_eq!(fs::read(&update.source).unwrap(), external);
    }

    #[test]
    fn refuses_identity_changes_and_malformed_updates() {
        for changed in [auth("account-2", "new"), b"not json".to_vec()] {
            let (_dir, update) = fixture();
            let prepared = prepare(std::slice::from_ref(&update)).unwrap();
            fs::write(&update.attempt, changed).unwrap();

            let warnings = persist(&prepared);
            assert_eq!(warnings.len(), 1);
            assert_eq!(fs::read(&update.source).unwrap(), auth("account-1", "old"));
        }
    }

    #[test]
    fn rejects_an_attempt_copy_that_differs_before_execution() {
        let (_dir, update) = fixture();
        fs::write(&update.attempt, auth("account-1", "different")).unwrap();
        let error = prepare(std::slice::from_ref(&update)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs before the harness starts")
        );
    }

    #[test]
    fn replacing_a_symlink_does_not_write_outside_the_credential_directory() {
        let (dir, update) = fixture();
        let prepared = prepare(std::slice::from_ref(&update)).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&update.source).unwrap();
        symlink(&outside, &update.source).unwrap();
        fs::write(&update.attempt, auth("account-1", "new")).unwrap();

        let warnings = persist(&prepared);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not a regular file"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }
}
