// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Out-of-band GitHub Actions runs on a pull request head. A maintainer may start a workflow by
//! hand on the source branch, or a scheduled or dispatched workflow may run on the same commit.
//! Such runs do not verify the pull request; GitHub leaves them out of the pull request's check
//! rollup, and so does the delivery gate. A rejected manual run would otherwise hold delivery
//! forever, because nothing reruns it.
//!
//! The workflow event is only visible through the Actions API. Without Actions read permission
//! every check run keeps counting, which is the stricter behavior.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::app::LACKS_PERMISSIONS;
use super::{CheckRun, GithubForge, MAX_PAGES};
use crate::{ForgeError, Result};

/// Events a pull request cannot trigger on its own head: they come from a person, a schedule or
/// an external API call, never from the change under review.
const OUT_OF_BAND_EVENTS: [&str; 3] = ["workflow_dispatch", "schedule", "repository_dispatch"];
const ACTIONS_APP: &str = "github-actions";

#[derive(Deserialize)]
struct RunPage {
    total_count: usize,
    workflow_runs: Vec<Run>,
}

#[derive(Deserialize)]
struct Run {
    id: u64,
    head_sha: String,
    event: String,
    check_suite_id: Option<u64>,
    repository: Repo,
}

#[derive(Deserialize)]
struct Repo {
    full_name: String,
}

/// Check suites on `head` that belong to out-of-band Actions runs.
pub(super) fn out_of_band_suites(
    forge: &GithubForge,
    remote: &str,
    head: &str,
    runs: &[CheckRun],
) -> Result<BTreeSet<u64>> {
    let mut excluded = BTreeSet::new();
    if !runs.iter().any(|run| {
        run.check_suite.is_some() && run.app.as_ref().is_some_and(|app| app.slug == ACTIONS_APP)
    }) {
        return Ok(excluded);
    }
    let token = match forge.actions_token(remote) {
        Ok(token) => token,
        Err(ForgeError::Other(message)) if message == LACKS_PERMISSIONS => return Ok(excluded),
        Err(error) => return Err(error),
    };
    let mut seen = 0;
    let mut expected = None;
    for page in 1..=MAX_PAGES {
        let batch: RunPage = forge.http().get(
            &format!("/repos/{remote}/actions/runs?head_sha={head}&per_page=100&page={page}"),
            Some(&token),
            false,
        )?;
        if expected.is_some_and(|count| count != batch.total_count)
            || batch.workflow_runs.iter().any(|run| {
                run.id == 0
                    || run.head_sha != head
                    || !run.repository.full_name.eq_ignore_ascii_case(remote)
            })
        {
            return Err(ForgeError::Other(
                "GitHub workflow runs changed during inspection; retry".into(),
            ));
        }
        expected = Some(batch.total_count);
        let empty = batch.workflow_runs.is_empty();
        seen += batch.workflow_runs.len();
        excluded.extend(
            batch
                .workflow_runs
                .into_iter()
                .filter(|run| OUT_OF_BAND_EVENTS.contains(&run.event.as_str()))
                .filter_map(|run| run.check_suite_id),
        );
        if seen == batch.total_count {
            break;
        }
        // An incomplete listing could keep a failure or hide which suites are out of band.
        if empty || seen > batch.total_count || page == MAX_PAGES {
            return Err(ForgeError::Other(
                "GitHub workflow-run listing is incomplete".into(),
            ));
        }
    }
    // Only suites created by GitHub Actions are matched; another app reusing an id never is.
    Ok(runs
        .iter()
        .filter(|run| run.app.as_ref().is_some_and(|app| app.slug == ACTIONS_APP))
        .filter_map(|run| run.check_suite.as_ref().map(|suite| suite.id))
        .filter(|id| excluded.contains(id))
        .collect())
}
