// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The deployment secret mechanism for v1: one file per `credential_ref` in a directory the
//! orchestrator can read. Values are trimmed. Configuration only ever names the ref.

use std::path::{Path, PathBuf};

use crate::{OrchestratorError, Result};

#[derive(Clone, Debug)]
pub struct SecretsDir {
    dir: PathBuf,
}

impl SecretsDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn get(&self, credential_ref: &str) -> Result<String> {
        if credential_ref.is_empty()
            || credential_ref.contains('/')
            || credential_ref.starts_with('.')
        {
            return Err(OrchestratorError::Invalid(format!(
                "credential_ref {credential_ref:?} is not a plain name"
            )));
        }
        let path = self.dir.join(credential_ref);
        let text = std::fs::read_to_string(&path)
            .map_err(|source| OrchestratorError::Io { path, source })?;
        let value = text.trim();
        if value.is_empty() {
            return Err(OrchestratorError::Invalid(format!(
                "secret {credential_ref} is empty"
            )));
        }
        Ok(value.to_owned())
    }
}
