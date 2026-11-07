// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! ArtiDezi configuration.
//!
//! Configuration is an explicit, version-controlled YAML document. Every security-relevant setting
//! must be spelled out; the validator rejects incomplete profiles instead of inheriting operational
//! defaults. Plaintext secrets never appear here: fields named `credential_ref` are opaque names
//! that the deployment's secret mechanism resolves. Optional UI initial password hashes are
//! authentication material and require private configuration handling.

pub mod model;
pub mod resolve;
pub mod validate;

use std::path::Path;

pub use model::Config;
pub use resolve::{ResolvedProjectPolicy, ResolvedRepository};
pub use validate::{Problem, validate};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse configuration: {0}")]
    Parse(#[from] serde_saphyr::Error),
    #[error("configuration is invalid:\n{}", .0.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<Problem>),
}

/// Parses and validates a configuration file. Returns every problem found rather than the first.
pub fn load(path: &Path) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse(&text)
}

/// YAML parsing options for every ArtiDezi document: an unknown tag is a mistake, not something to ignore.
pub fn yaml_options() -> serde_saphyr::Options {
    let mut options = serde_saphyr::Options::default();
    options.reject_unsupported_tags = true;
    options
}

pub fn parse(text: &str) -> Result<Config, LoadError> {
    let config: Config = serde_saphyr::from_str_with_options(text, yaml_options())?;
    let problems = validate(&config);
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(LoadError::Invalid(problems))
    }
}
