// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::collections::BTreeSet;

fn is_watched(config: &Config, source: &TrackedPullRequest) -> bool {
    config.projects.get(&source.project).is_some_and(|project| {
        project.pull_requests.as_ref().is_some_and(|discovery| {
            discovery.repositories.contains(&source.repository_id)
                && project
                    .repositories
                    .get(&source.repository_id)
                    .is_some_and(|repository| {
                        repository.forge == source.forge && repository.remote == source.remote
                    })
        })
    })
}

pub(in super::super) fn incoming_url(query: &TaskQuery) -> String {
    format!(
        "/incoming-changes?project={}&filter={}&q={}&page={}",
        url(&query.project),
        url(&query.filter),
        url(&query.q),
        query.page.max(1)
    )
}

fn scope_fields(query: &TaskQuery) -> String {
    format!(
        r#"<input type="hidden" name="project" value="{}"><input type="hidden" name="filter" value="{}"><input type="hidden" name="q" value="{}"><input type="hidden" name="page" value="{}">"#,
        e(&query.project),
        e(&query.filter),
        e(&query.q),
        query.page.max(1)
    )
}

// Stored status messages describe scheduler decisions. Present the current UI terminology
// without rewriting historical records or changing the scheduler's state comparisons.
pub(super) fn source_status(source: &TrackedPullRequest, settle_seconds: Option<u32>) -> String {
    if matches!(
        source.status.as_str(),
        "Waiting for the source head to settle" | "Waiting for settle window"
    ) {
        return settle_seconds.map_or_else(
            || "Waiting briefly to confirm the PR is stable".into(),
            |seconds| {
                let minutes = seconds.div_ceil(60).max(1);
                format!(
                    "Waiting briefly (up to {minutes} minute{}) to confirm the PR is stable",
                    if minutes == 1 { "" } else { "s" }
                )
            },
        );
    }
    source
        .status
        .replace("Shadow assessment", "Dry-run evaluation")
        .replace("shadow assessment", "dry-run evaluation")
        .replace("assessments", "evaluations")
        .replace("Source PR", "Source change")
        .replace("the PR", "the change")
}

pub(in super::super) fn incoming_changes(
    config: &Config,
    sources: &[TrackedPullRequest],
    tasks: &[Task],
    token: &Session,
    query: &TaskQuery,
    access: &Access,
) -> String {
    let names: BTreeSet<_> = config
        .projects
        .iter()
        .filter(|(_, p)| p.pull_requests.is_some())
        .map(|(name, _)| name.as_str())
        .chain(sources.iter().map(|source| source.project.as_str()))
        .chain((!query.project.is_empty()).then_some(query.project.as_str()))
        .collect();
    let enabled = config.projects.iter().any(|(name, p)| {
        p.pull_requests.is_some() && (query.project.is_empty() || *name == query.project)
    });
    let mut body = search_form(
        "/incoming-changes",
        "Search changes or repositories",
        query,
        Some(names.into_iter().collect()),
    );
    body.push_str(
        r#"<p id="scan-status" class="scan-status" role="status" aria-live="polite" hidden></p>"#,
    );
    body.push_str(r#"<section class="task-board" id="live-content" data-refresh="true"><div class="incoming-controls">"#);
    let search = query.q.trim().to_lowercase();
    let mut matching: Vec<_> = sources
        .iter()
        .filter(|source| {
            let task = tasks
                .iter()
                .find(|task| Some(&task.id) == source.last_task_id.as_ref());
            (query.project.is_empty() || query.project == source.project)
                && matches_source(source, task, &search)
        })
        .collect();
    matching.sort_by(|a, b| {
        b.head_observed_at
            .cmp(&a.head_observed_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    let in_filter = |source: &&TrackedPullRequest, filter: &str| match filter {
        "paused" => source.paused && !source_is_closed(source),
        "history" => source_is_closed(source),
        _ => !source_is_closed(source) && !source.paused,
    };
    let filter = if ["paused", "history"].contains(&query.filter.as_str()) {
        query.filter.as_str()
    } else {
        ""
    };
    write!(body, r#"<form id="task-filters" class="tabs" method="get" action="/incoming-changes" aria-label="Filter incoming changes"><input type="hidden" name="q" value="{}"><input type="hidden" name="project" value="{}">"#, e(&query.q), e(&query.project)).unwrap();
    for (key, label, symbol) in [
        ("", "Open", "eye"),
        ("paused", "Paused", "alert"),
        ("history", "Closed", "closed"),
    ] {
        let count = matching
            .iter()
            .filter(|source| in_filter(source, key))
            .count();
        let tone = if key.is_empty() { "open" } else { key };
        write!(body, r#"<button type="submit" name="filter" value="{key}" class="filter-tab {}" aria-pressed="{}">{}{label}<span class="filter-count {} {}">{count}</span></button>"#, if filter == key { "active" } else { "" }, filter == key, icon(symbol), if count > 0 { "nonzero" } else { "" }, tone).unwrap();
    }
    body.push_str("</form>");
    if enabled
        && (if query.project.is_empty() {
            access.is_admin()
        } else {
            access.can_write(&query.project)
        })
    {
        write!(body, r#"<form class="scan-incoming" method="post" action="/incoming-changes/scan">{}{}<button class="button primary" title="Scan the selected project, or all configured projects when none is selected">{}Scan now</button></form>"#, csrf(token), scope_fields(query), icon("search")).unwrap();
    }
    body.push_str("</div><div class=\"discovery-list\">");
    let filtered: Vec<_> = matching
        .into_iter()
        .filter(|source| in_filter(source, filter))
        .collect();
    const PAGE_SIZE: usize = 25;
    let pages = filtered.len().div_ceil(PAGE_SIZE).max(1);
    let page = query.page.clamp(1, pages);
    if filtered.is_empty() {
        write!(body, r#"<div class="empty-state"><h2>No matching changes</h2><p>{}</p></div>"#,
            if enabled { "Only changes matched by configured project rules appear here." }
            else { "Discovery is off for this selection. Configure a project's rules to start evaluating incoming changes." }).unwrap();
    }
    for source in filtered.iter().skip((page - 1) * PAGE_SIZE).take(PAGE_SIZE) {
        let task = tasks
            .iter()
            .find(|task| Some(&task.id) == source.last_task_id.as_ref());
        let closed = source_is_closed(source);
        let watching = is_watched(config, source);
        let (label, tone, symbol, explanation) = evaluation_status(source, task, watching);
        let link = if config.forges.contains_key(&source.forge) {
            Some(format!(
                "{}/pull/{}",
                repository_url(config, &source.forge, &source.remote),
                source.number
            ))
        } else if source.url.starts_with("https://") || source.url.starts_with("http://") {
            Some(source.url.clone())
        } else {
            None
        };
        let source_title = e(&short(source_title(source, task), 220));
        let repository = if config.forges.contains_key(&source.forge) {
            let repository = repository_url(config, &source.forge, &source.remote);
            format!(
                r#"<a class="source-repository" href="{}" target="_blank" rel="noopener noreferrer">{}<span>{}</span></a>"#,
                e(&repository),
                icon("forge"),
                e(&source.remote)
            )
        } else {
            format!(
                r#"<span class="source-repository">{}<span>{}</span></span>"#,
                icon("forge"),
                e(&source.remote)
            )
        };
        let reference = if let Some(link) = link.as_deref() {
            let link = e(link);
            format!(
                r##"<a class="source-number" href="{link}" target="_blank" rel="noopener noreferrer">#{}</a> <span class="source-title-group">(<a class="source-title" href="{link}" target="_blank" rel="noopener noreferrer">{source_title}</a>)</span>"##,
                source.number
            )
        } else {
            format!(
                r#"<span class="source-number">#{}</span> <span class="source-title-group">(<span class="source-title">{source_title}</span>)</span>"#,
                source.number
            )
        };
        let settle_seconds = config
            .projects
            .get(&source.project)
            .and_then(|project| project.pull_requests.as_ref())
            .map_or(0, |discovery| discovery.settle_seconds);
        let explanation = if matches!(
            source.status.as_str(),
            "Waiting for the source head to settle" | "Waiting for settle window"
        ) {
            format!(
                "ArtiDezi waits up to {} seconds after observing a new head so a burst of commits can settle before evaluation",
                settle_seconds
            )
        } else {
            explanation.to_owned()
        };
        write!(body, r#"<article class="task-row discovery-source row-tone-{tone}" id="source-{}"><div class="task-row-main source-main"><h3 class="source-heading">{repository} {reference}</h3><div class="task-meta source-meta"><a class="source-project text-link" href="/incoming-changes?project={}" title="Project">{}{}</a>{}</div>"#, e(&source.id), url(&source.project), icon("project"), e(&source.project), delivery_badge(task.map(|task| task.delivery_mode).unwrap_or_else(|| {
            config.projects.get(&source.project).and_then(|p| p.pull_requests.as_ref()).and_then(|d| d.handlers.iter().find(|h| h.id == source.handler_id)).map_or(DeliveryMode::Shadow, |h| if h.mode == artidezi_config::model::PullRequestMode::Publish { DeliveryMode::MergeRequest } else { DeliveryMode::Shadow })
        }))).unwrap();
        if !closed
            && watching
            && ![
                "This head has already been evaluated",
                "An evaluation is already queued or running",
                "Automatic assessments paused by a human",
            ]
            .contains(&source.status.as_str())
            && !source.status.starts_with("Shadow assessment recorded;")
            && !(label == "Ready for review"
                && source.status == "Published adaptations await review")
        {
            write!(
                body,
                "<p class=\"source-reason muted\">{}</p>",
                e(&source_status(source, Some(settle_seconds)))
            )
            .unwrap();
        }
        body.push_str("</div><div class=\"task-row-end source-end\">");
        if source.paused
            && task.is_some_and(|t| t.state == TaskState::Working)
            && watching
            && !closed
        {
            write!(body, r#"<span class="badge" title="Future evaluations are paused; the running evaluation can finish">{}Evaluations paused</span>"#, icon("pause")).unwrap();
        }
        body.push_str("<div class=\"source-actions\">");
        if let Some(task) = task {
            write!(
                body,
                r#"<a class="button secondary" href="/tasks/{}">{}Task #{}</a>"#,
                task.number,
                icon("tasks"),
                task.number
            )
            .unwrap();
        }
        if !closed && watching && access.can_write(&source.project) {
            let (action, label, tone, symbol, note) = if source.paused {
                (
                    "resume",
                    "Resume evaluations",
                    "positive",
                    "play",
                    "Allows automatic evaluation again and resets this change's evaluation budget.",
                )
            } else {
                (
                    "pause",
                    "Pause evaluations",
                    "warning",
                    "pause",
                    "Stops automatic reevaluation of this change and cancels queued evaluations.",
                )
            };
            let active = source.paused && task.is_some_and(|t| t.state == TaskState::Working);
            let disabled = if active { "disabled" } else { "" };
            let note = if active {
                "Available when the running evaluation finishes"
            } else {
                note
            };
            write!(body, r#"<form method="post" action="/incoming-changes/{}/{action}">{}{}<button class="button {tone}" title="{note}" {disabled}>{}{label}</button></form>"#, url(&source.id), csrf(token), scope_fields(query), icon(symbol)).unwrap();
        }
        write!(
            body,
            r#"</div><span class="status {tone}" title="{}">{}{label}</span></div></article>"#,
            e(&explanation),
            icon(symbol)
        )
        .unwrap();
    }
    body.push_str("</div>");
    if pages > 1 {
        write!(body, r#"<nav class="pagination" aria-label="Incoming change pages"><span>Page {page} of {pages} · {} changes</span><div>"#, filtered.len()).unwrap();
        for (number, label, rel) in [
            (page.saturating_sub(1), "Previous", "prev"),
            (page + 1, "Next", "next"),
        ] {
            if number > 0 && number <= pages {
                write!(body, r#"<a class="button secondary" rel="{rel}" href="/incoming-changes?project={}&amp;filter={}&amp;q={}&amp;page={number}">{label}</a>"#, url(&query.project), url(filter), url(&query.q)).unwrap();
            }
        }
        body.push_str("</div></nav>");
    }
    body.push_str("</section>");
    root_shell("Incoming changes", token, "incoming", "", &body)
}

// A hold prevents future work but does not stop an already leased evaluation.
fn evaluation_status(
    source: &TrackedPullRequest,
    task: Option<&Task>,
    watching: bool,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if source_is_closed(source) {
        (
            "Closed or merged",
            "closed",
            "closed",
            "The source change is closed; no further automatic evaluations will start",
        )
    } else if !watching {
        (
            "Discovery off",
            "closed",
            "minus",
            "This repository is no longer watched by the project's rules",
        )
    } else if task.is_some_and(|t| t.state == TaskState::Working) {
        (
            "In progress",
            "working",
            "clock",
            "An evaluation task is queued or running",
        )
    } else if task.is_some_and(|t| t.state == TaskState::NeedsHuman) {
        (
            "Needs attention",
            "needs_human",
            "alert",
            "The latest evaluation needs human input",
        )
    } else if source.paused {
        (
            "Paused",
            "needs_human",
            "pause",
            "Automatic evaluations are paused for this change",
        )
    } else if task.is_some_and(|t| t.state == TaskState::WaitingForCi) {
        (
            "Waiting for CI",
            "waiting_for_ci",
            "clock",
            "Delivery continues automatically when checks pass and the assessment is still current",
        )
    } else if task.is_some_and(|t| t.state == TaskState::AwaitingReview) {
        (
            "Ready for review",
            "awaiting_review",
            "eye",
            "Published adaptations are waiting for human review",
        )
    } else if task.is_some_and(|t| {
        t.state == TaskState::Done
            && source.last_task_id.as_deref() == Some(&t.id)
            && artidezi_orchestrator::discovery::source_of(t)
                .ok()
                .flatten()
                .is_some_and(|frozen| {
                    frozen.tracking_id == source.id && frozen.input.commit.0 == source.observed_head
                })
    }) || source.status == "This head has already been evaluated"
        || source.status.starts_with("Shadow assessment recorded;")
    {
        (
            "Evaluated",
            "done",
            "check",
            "The latest observed source revision has a completed evaluation; open its task for the result",
        )
    } else if source.status == "Forge checks failed; evaluation blocked" {
        (
            "Forge checks failed",
            "needs_human",
            "alert",
            "GitHub checks are failing, so ArtiDezi has not started an evaluation",
        )
    } else {
        (
            "Waiting",
            "closed",
            "clock",
            "Waiting for the project's rules, timing or capacity before evaluation",
        )
    }
}

// Repository + number links must identify one source, not also #10, #100, etc.
fn matches_source(source: &TrackedPullRequest, task: Option<&Task>, search: &str) -> bool {
    if let Some((remote, number)) = search.rsplit_once(" #")
        && let Ok(number) = number.parse::<u64>()
    {
        return source.remote.to_lowercase() == remote && source.number == number;
    }
    format!(
        "{} {} #{} {}",
        source.project,
        source.remote,
        source.number,
        source_title(source, task)
    )
    .to_lowercase()
    .contains(search)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Task, TrackedPullRequest) {
        let now = chrono::Utc::now();
        (
            Task {
                id: "tsk_example".into(),
                number: 12,
                project: "application".into(),
                kind: TaskKind::PullRequest,
                created_at: now,
                updated_at: now,
                created_by: "discovery".into(),
                instructions: "Evaluate update".into(),
                delivery_mode: DeliveryMode::Shadow,
                agent_profile: "default".into(),
                policy_snapshot: "{}".into(),
                policy_digest: String::new(),
                state: TaskState::Working,
                repositories: vec![],
            },
            TrackedPullRequest {
                id: "pr_example".into(),
                project: "application".into(),
                repository_id: "app".into(),
                forge: "forge".into(),
                remote: "org/app".into(),
                number: 1,
                url: "https://forge.example/org/app/pulls/1".into(),
                title: "Update library".into(),
                closed: false,
                handler_id: "updates".into(),
                observed_head: "a".repeat(40),
                head_observed_at: now,
                last_task_id: Some("tsk_example".into()),
                runs: 1,
                paused: false,
                status: "Waiting for settle window".into(),
                follow_up: None,
            },
        )
    }

    #[test]
    fn lifecycle_distinguishes_waiting_work_holds_and_completed_heads() {
        let (mut task, mut source) = fixture();
        assert_eq!(evaluation_status(&source, None, true).0, "Waiting");
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "In progress"
        );
        source.paused = true;
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "In progress"
        );
        task.state = TaskState::NeedsHuman;
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "Needs attention"
        );
        task.state = TaskState::Done;
        assert_eq!(evaluation_status(&source, Some(&task), true).0, "Paused");
        source.paused = false;
        task.state = TaskState::WaitingForCi;
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "Waiting for CI"
        );
        task.state = TaskState::AwaitingReview;
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "Ready for review"
        );
        task.state = TaskState::Done;
        source.status = "This head has already been evaluated".into();
        assert_eq!(evaluation_status(&source, Some(&task), true).0, "Evaluated");
        source.status = "Waiting for settle window".into();
        // A previous completed task does not mark a new source revision complete.
        assert_eq!(evaluation_status(&source, Some(&task), true).0, "Waiting");
        assert_eq!(
            evaluation_status(&source, Some(&task), false).0,
            "Discovery off"
        );
        source.closed = true;
        assert_eq!(
            evaluation_status(&source, Some(&task), true).0,
            "Closed or merged"
        );
    }

    #[test]
    fn completed_task_marks_only_its_frozen_source_head_as_evaluated() {
        let (mut task, mut source) = fixture();
        let config =
            artidezi_config::parse(include_str!("../../../config/tests/fixtures/valid.yaml"))
                .unwrap();
        let mut policy = config
            .resolve_project("acme-roles", &TaskSelection::default(), vec![])
            .unwrap();
        policy.source_pull_request = Some(
            serde_json::from_value(serde_json::json!({
                "tracking_id": source.id,
                "repository_id": source.repository_id,
                "number": source.number,
                "url": source.url,
                "author": "update-bot",
                "source_branch": "updates/library",
                "target_branch": "main",
                "handler_id": source.handler_id,
                "input": {
                    "name": "source",
                    "commit": source.observed_head,
                    "bundle": { "sha256": "0".repeat(64), "size": 0 }
                }
            }))
            .unwrap(),
        );
        task.policy_snapshot = serde_json::to_string(&policy).unwrap();
        task.state = TaskState::Done;
        source.status = "Assessment review published".into();
        assert_eq!(evaluation_status(&source, Some(&task), true).0, "Evaluated");

        source.observed_head = "b".repeat(40);
        source.status = "Waiting for settle window".into();
        assert_eq!(evaluation_status(&source, Some(&task), true).0, "Waiting");
    }

    #[test]
    fn origin_links_find_exact_sources_and_manual_tasks_have_no_badge() {
        let (mut task, mut source) = fixture();
        assert!(task_origin(&task, None, false).contains("Incoming change"));
        assert!(task_origin(&task, None, false).contains("class=\"task-origin compact incoming\""));
        assert!(!task_origin(&task, None, false).contains("Incoming change</span>"));
        assert!(!task_origin(&task, None, false).contains("<a "));
        let link = task_origin(&task, Some(&source), true);
        assert!(link.contains("q=org%2Fapp%20%231"));
        assert!(link.contains("#source-pr_example"));
        assert!(matches_source(&source, Some(&task), "org/app #1"));
        assert!(matches_source(&source, Some(&task), "update library"));
        source.number = 10;
        assert!(!matches_source(&source, Some(&task), "org/app #1"));
        source.closed = true;
        assert!(task_origin(&task, Some(&source), true).contains("filter=history"));
        task.kind = TaskKind::Manual;
        assert!(task_origin(&task, None, false).contains("class=\"task-origin compact manual\""));
        assert!(task_origin(&task, None, false).contains("Manual task"));
        assert!(task_origin(&task, Some(&source), true).is_empty());
    }
}
