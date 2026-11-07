// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Scripted harness for the offline test suite. It runs an executable the test provides, with the
//! same environment a real harness would get, so orchestration can be proven without a model.

use std::path::PathBuf;

use artidezi_contract::job::Harness;

use crate::{HarnessAdapter, Invocation, InvocationContext, Result, TranscriptFacts};

pub struct FakeHarness {
    pub script: PathBuf,
}

impl HarnessAdapter for FakeHarness {
    fn harness(&self) -> Harness {
        Harness::Fake
    }

    fn installed_version(&self) -> Result<String> {
        Ok("fake-1".into())
    }

    fn build_invocation(&self, ctx: &InvocationContext<'_>) -> Result<Invocation> {
        let mut env = crate::common_env(ctx);
        env.push((
            "ARTIDEZI_FAKE_MODEL".into(),
            ctx.job.agent_profile.model.clone(),
        ));
        Ok(Invocation {
            program: self.script.clone(),
            args: vec![],
            env,
            cwd: ctx.primary_repo_dir.to_path_buf(),
            stdin: None,
            credential_updates: vec![],
        })
    }

    fn read_transcript(&self, stdout: &[u8]) -> TranscriptFacts {
        // The fake prints optional `key=value` lines so tests can exercise transcript parsing.
        let mut facts = TranscriptFacts::default();
        for line in String::from_utf8_lossy(stdout).lines() {
            match line.split_once('=') {
                Some(("session_id", v)) => facts.session_id = Some(v.to_owned()),
                Some(("model", v)) => facts.model = Some(v.to_owned()),
                Some(("input_tokens", v)) => facts.input_tokens = v.parse().ok(),
                Some(("output_tokens", v)) => facts.output_tokens = v.parse().ok(),
                _ => {}
            }
        }
        facts
    }
}
