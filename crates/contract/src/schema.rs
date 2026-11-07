// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! JSON Schema export, so non-Rust harness adapters and the prompt template can reference the
//! exact document shapes.

use schemars::{Schema, schema_for};

use crate::{AgentResult, AttemptRecord, Job};

pub fn job_schema() -> Schema {
    schema_for!(Job)
}

pub fn result_schema() -> Schema {
    schema_for!(AgentResult)
}

pub fn attempt_schema() -> Schema {
    schema_for!(AttemptRecord)
}
