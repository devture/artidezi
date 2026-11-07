// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The versioned documents that cross ArtiDezi's trust boundaries.
//!
//! Four documents exist:
//!
//! - [`job::Job`] (`job.json`): orchestrator → worker supervisor, non-secret attempt description;
//! - `task.md`: orchestrator → agent, rendered prompt (plain Markdown, not typed here);
//! - [`result::AgentResult`] (`result.json`): agent → supervisor → orchestrator;
//! - [`attempt::AttemptRecord`] (`attempt.json`): supervisor → orchestrator.
//!
//! Every document carries `schema_version`. The orchestrator re-validates every document it receives,
//! regardless of what the supervisor already checked, because a full-access job can steal the
//! supervisor's identity.

pub mod attempt;
pub mod common;
pub mod job;
pub mod result;
pub mod schema;
pub mod transient;
pub mod validate;
pub mod wire;

pub use attempt::AttemptRecord;
pub use common::SCHEMA_VERSION;
pub use job::Job;
pub use result::AgentResult;

/// Application release version, independent of Cargo package and document schema versions.
///
/// Release builds embed it through `ARTIDEZI_VERSION`; every other build reports `dev`.
pub fn app_version() -> &'static str {
    match option_env!("ARTIDEZI_VERSION") {
        Some(version) => version,
        None => "dev",
    }
}
