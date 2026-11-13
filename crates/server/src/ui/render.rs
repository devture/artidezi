// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

#[path = "attempt.rs"]
mod attempt;
pub(super) use attempt::{missing_result_diagnostic, unschedulable_reason};
#[path = "incoming.rs"]
mod incoming;
pub(super) use incoming::{incoming_changes, incoming_url};

#[path = "diff.rs"]
mod diff;
#[path = "markdown.rs"]
mod markdown;

use super::*;
use std::fmt::Write;

fn e(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn url(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            write!(out, "%{byte:02X}").unwrap();
        }
    }
    out
}
fn profile_link(name: &str) -> String {
    format!(
        r#"<a class="badge agent-link" href="/agent-profiles#{}" title="Agent profile">{}{}</a>"#,
        url(name),
        icon("agent"),
        e(name)
    )
}
fn task_origin(task: &Task, tracking: Option<&TrackedPullRequest>, linked: bool) -> String {
    if task.kind == TaskKind::Manual {
        return if linked {
            String::new()
        } else {
            format!(
                r#"<span class="task-origin compact manual" title="Manual task" aria-label="Manual task">{}</span>"#,
                icon("account")
            )
        };
    }
    let content = format!("{}Incoming change", icon("incoming"));
    let note = "Created from an incoming change matched by a project rule";
    if linked {
        let target = tracking.map_or_else(
            || format!("/incoming-changes?project={}", url(&task.project)),
            |source| {
                format!(
                    "{}#source-{}",
                    incoming_url(&TaskQuery {
                        project: source.project.clone(),
                        filter: if source_is_closed(source) {
                            "history".into()
                        } else {
                            String::new()
                        },
                        q: format!("{} #{}", source.remote, source.number),
                        ..Default::default()
                    }),
                    url(&source.id)
                )
            },
        );
        format!(
            r#"<a class="badge task-origin" href="{}" title="{note}">{content}</a>"#,
            e(&target)
        )
    } else {
        format!(
            r#"<span class="task-origin compact incoming" title="Incoming change" aria-label="Incoming change">{}</span>"#,
            icon("incoming")
        )
    }
}
fn short(value: &str, max: usize) -> String {
    let s: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        format!("{s}…")
    } else {
        s
    }
}
fn icon(name: &str) -> String {
    let paths = match name {
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "star" => {
            r#"<path d="m12 3 2.8 5.7 6.2.9-4.5 4.4 1.1 6.2-5.6-2.9-5.6 2.9 1.1-6.2L3 9.6l6.2-.9Z"/>"#
        }
        "arrow" | "external" => r#"<path d="M7 17 17 7M7 7h10v10"/>"#,
        "tasks" => {
            r#"<rect x="5" y="3" width="14" height="18" rx="3"/><path d="m8 9 1 1 2-2m2 1h3m-8 6 1 1 2-2m2 1h3"/>"#
        }
        "project" => r#"<path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v10H3Z"/>"#,
        "eye" => {
            r#"<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>"#
        }
        "forge" => {
            r#"<rect x="3" y="3" width="18" height="18" rx="4"/><circle cx="8" cy="8" r="1.5"/><circle cx="8" cy="16" r="1.5"/><circle cx="16" cy="8" r="1.5"/><path d="M8 9.5v5m1.5-2.5H12a4 4 0 0 0 4-2.5"/>"#
        }
        "pause" => r#"<path d="M8 5v14M16 5v14"/>"#,
        "play" => r#"<path d="m7 4 13 8-13 8Z"/>"#,
        "patch" | "branch" => {
            r#"<circle cx="6" cy="4" r="2"/><circle cx="6" cy="20" r="2"/><circle cx="18" cy="4" r="2"/><path d="M6 6v12m0-6h6a6 6 0 0 0 6-6"/>"#
        }
        "edit" => r#"<path d="m15 4 5 5M4 20l5-1L21 7a2 2 0 0 0-4-4L5 15Z"/>"#,
        "merge-request" => {
            r#"<circle cx="6" cy="6" r="3"/><path d="M6 9v12"/><circle cx="18" cy="18" r="3"/><path d="M18 15V6a3 3 0 0 0-3-3h-1"/>"#
        }
        "merge" => r#"<path d="M6 3v6a6 6 0 0 0 6 6m6-12v6a6 6 0 0 1-6 6m0 0v6m-3-3 3 3 3-3"/>"#,
        "incoming" => {
            r#"<path d="M4 13h4l2 4h4l2-4h4M4 13l2-9h12l2 9v7H4Z"/><path d="M12 3v8m-3-3 3 3 3-3"/>"#
        }
        "push" => r#"<path d="M12 17V3m-5 5 5-5 5 5M4 15v6h16v-6"/>"#,
        "agent" => {
            r#"<rect x="4" y="6" width="16" height="14" rx="4"/><path d="M12 3v3M8 11v2m8-2v2m-7 3h6M1 11v4m22-4v4"/>"#
        }
        "worker" => {
            r#"<rect x="3" y="4" width="18" height="14" rx="2"/><path d="M8 21h8M12 18v3M8 9h.01M12 9h4M8 13h8"/>"#
        }
        "stdout" => {
            r#"<rect x="3" y="4" width="18" height="16" rx="3"/><path d="m7 9 3 3-3 3m6 0h4"/>"#
        }
        "stderr" => r#"<path d="m12 3 10 17H2Z"/><path d="M12 9v4m0 3v1"/>"#,
        "file" => r#"<path d="M14 3H5v18h14V8Zm0 0v5h5M8 12h8m-8 4h6"/>"#,
        "clock" => r#"<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>"#,
        "minus" => r#"<path d="M5 12h14"/>"#,
        "closed" => r#"<circle cx="12" cy="12" r="9"/><path d="m9 9 6 6m0-6-6 6"/>"#,
        "check" => r#"<path d="m5 12 4 4L19 6"/>"#,
        "logout" => r#"<path d="M9 5H4v14h5m4-14 7 7-7 7m7-7H8"/>"#,
        "account" => r#"<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/>"#,
        "search" => r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 4 4"/>"#,
        "back" => r#"<path d="m12 5-7 7 7 7M5 12h15"/>"#,
        "shield" => r#"<path d="m12 3 8 3v6c0 4-8 9-8 9s-8-5-8-9V6Z"/><path d="m8 12 3 3 5-6"/>"#,
        _ => r#"<circle cx="12" cy="12" r="9"/><path d="M12 7v6m0 3v1"/>"#,
    };
    let class = match name {
        "forge" => "icon forge-icon",
        "external" => "icon external-icon",
        _ => "icon",
    };
    format!(
        r#"<svg class="{class}" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{paths}</svg>"#
    )
}
fn brand() -> String {
    format!(
        r#"<a class="brand" href="/tasks" aria-label="ArtiDezi home"><img class="brand-mark" src="{}" width="40" height="40" alt=""><span>ArtiDezi<span class="brand-period">.</span></span></a>"#,
        assets::LOGO.url
    )
}
fn document(title: &str, content: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light"><title>{} · ArtiDezi</title><link rel="stylesheet" href="{}"><link rel="icon" type="image/svg+xml" href="{}"><script defer src="{}"></script></head><body>{content}</body></html>"#,
        e(title),
        assets::CSS.url,
        assets::LOGO.url,
        assets::JS.url
    )
}
fn csrf(value: &Session) -> String {
    format!(
        r#"<input type="hidden" name="csrf" value="{}">"#,
        e(&value.csrf)
    )
}
fn shell(title: &str, _config: &Config, token: &Session, active: &str, content: &str) -> String {
    let (parent, parent_url) = match active {
        "projects" => ("Projects", "/projects"),
        "agents" => ("Agent profiles", "/agent-profiles"),
        "workers" => ("Workers", "/workers"),
        _ => ("Tasks", "/tasks"),
    };
    let breadcrumb = if title == parent {
        format!(r#"<span aria-current="page">{parent}</span>"#)
    } else {
        format!(
            r#"<a href="{parent_url}">{parent}</a><span aria-hidden="true">/</span><span aria-current="page">{}</span>"#,
            e(title)
        )
    };
    shell_with_breadcrumb(title, token, active, content, &breadcrumb, title == parent)
}

#[derive(Clone, Copy)]
pub(super) struct NavCounts {
    tasks_attention: usize,
    incoming_open: usize,
}

pub(super) fn nav_counts(sources: &[TrackedPullRequest], tasks: &[Task]) -> NavCounts {
    NavCounts {
        tasks_attention: tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.state,
                    TaskState::NeedsHuman | TaskState::AwaitingReview
                )
            })
            .count(),
        incoming_open: sources
            .iter()
            .filter(|source| !source_is_closed(source) && !source.paused)
            .count(),
    }
}

/// Each tag keeps one colour everywhere, so a worker's tags and a project's can be matched by eye.
fn tag_badges(tags: &[String]) -> String {
    let badges = tags.iter().map(|tag| {
        let hash = tag.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
        format!(
            r#"<span class="badge tag tag-{}">{}</span>"#,
            hash % 8,
            e(tag)
        )
    });
    format!(
        r#"<span class="tag-list">{}</span>"#,
        badges.collect::<String>()
    )
}

fn shell_with_breadcrumb(
    title: &str,
    token: &Session,
    active: &str,
    content: &str,
    breadcrumb: &str,
    root_page: bool,
) -> String {
    let password_notice = if token.password_change_required {
        r#"<aside class="password-notice" role="status">You are still using an initial or reset password. <a href="/account#change-password">Change your password</a>.</aside>"#
    } else {
        ""
    };
    let header = if root_page {
        breadcrumb.to_string()
    } else {
        format!(r#"<nav class="breadcrumb" aria-label="Breadcrumb">{breadcrumb}</nav>"#)
    };
    let mut nav = String::new();
    for (key, label, symbol, path) in [
        ("tasks", "Tasks", "tasks", "/tasks"),
        (
            "incoming",
            "Incoming changes",
            "incoming",
            "/incoming-changes",
        ),
        ("projects", "Projects", "project", "/projects"),
        ("agents", "Agent profiles", "agent", "/agent-profiles"),
        ("workers", "Workers", "worker", "/workers"),
    ] {
        let count = match key {
            "tasks" => token.nav_counts.tasks_attention,
            "incoming" => token.nav_counts.incoming_open,
            _ => 0,
        };
        let badge = if matches!(key, "tasks" | "incoming") {
            format!(
                r#"<span class="nav-count {}" title="{}">{count}</span>"#,
                if count > 0 { "nonzero" } else { "" },
                match key {
                    "tasks" => "Tasks needing attention or review",
                    "incoming" => "Open incoming changes",
                    _ => "",
                }
            )
        } else {
            String::new()
        };
        write!(
            nav,
            r#"<a class="nav-link {}" href="{path}">{}{label}{badge}</a>"#,
            if active == key { "selected" } else { "" },
            icon(symbol)
        )
        .unwrap();
    }
    let account_current = if active == "account" {
        r#"aria-current="page""#
    } else {
        ""
    };
    let account_id = e(&token.access.id);
    document(
        title,
        &format!(
            r##"<a class="skip-link" href="#main">Skip to content</a><div class="app-shell" data-timezone="{}" data-timezone-initialized="{}"><header class="app-nav">{}<nav aria-label="Main navigation">{nav}</nav><div class="account"><a class="button nav-account" href="/account" aria-label="Account: {account_id}" title="Signed in as {account_id}" {account_current}>{}<span>{account_id}</span></a><form class="signout" method="post" action="/logout">{}<button class="button nav-signout">{}<span>Sign out</span></button></form></div></header><div class="main-wrap"><header class="topbar">{header}</header><main id="main" tabindex="-1">{password_notice}{content}</main><footer class="footer"><span>ArtiDezi</span><span>v{}</span></footer></div></div>"##,
            e(&token.access.timezone),
            token.access.timezone_initialized,
            brand(),
            icon("account"),
            csrf(token),
            icon("logout"),
            artidezi_contract::app_version()
        ),
    )
}
fn notice(value: &str) -> String {
    let text = match value {
        "created" => "Task created.",
        "revised" => "Revision requested.",
        "closed" => "Task closed.",
        _ => return String::new(),
    };
    format!(
        r#"<div class="notice" role="status">{}{text}</div>"#,
        icon("check")
    )
}
fn status_icon(state: TaskState) -> &'static str {
    match state {
        TaskState::Working => "clock",
        TaskState::WaitingForCi => "clock",
        TaskState::AwaitingReview => "eye",
        TaskState::NeedsHuman => "alert",
        TaskState::Done => "check",
        TaskState::Closed => "closed",
    }
}
fn status(task: &Task) -> String {
    let (label, description) = match task.state {
        TaskState::Working => ("In progress", "Queued or running with a worker."),
        TaskState::WaitingForCi => (
            "Waiting for CI",
            "Delivery continues automatically when checks pass and the assessment is still current.",
        ),
        TaskState::AwaitingReview => (
            "Ready for review",
            "Published changes are ready for human review.",
        ),
        TaskState::NeedsHuman => ("Needs attention", "Human input is needed to proceed."),
        TaskState::Done if task.delivery_mode == DeliveryMode::Shadow => {
            ("Completed", "Evaluation recorded; nothing was published.")
        }
        TaskState::Done => ("Completed", "Changes delivered."),
        TaskState::Closed => (
            "Closed",
            "Further task delivery is stopped. Existing PRs and history are kept.",
        ),
    };
    format!(
        r#"<span class="status {}" title="{description}">{}{label}</span>"#,
        task.state.as_str(),
        icon(status_icon(task.state))
    )
}
fn delivery_badge(value: DeliveryMode) -> String {
    let (key, label, note) = mode(value);
    let symbol = delivery_icon(value);
    format!(
        r#"<span class="badge delivery {key}" title="{note}">{}{label}</span>"#,
        icon(symbol)
    )
}
fn delivery_icon(value: DeliveryMode) -> &'static str {
    match value {
        DeliveryMode::Shadow => "search",
        DeliveryMode::MergeRequest => "merge-request",
        DeliveryMode::MergeRequestWithAutomerge => "merge",
        DeliveryMode::DirectPushFastForward => "push",
    }
}

fn time(at: &chrono::DateTime<chrono::Utc>, timezone: &str) -> String {
    let zone = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
    let local = at.with_timezone(&zone);
    let elapsed = (chrono::Utc::now() - *at).num_seconds();
    let label = if elapsed < -60 {
        format!("in {} min", (-elapsed + 59) / 60)
    } else if elapsed < 60 {
        "just now".to_owned()
    } else if elapsed < 3_600 {
        format!("{} min ago", elapsed / 60)
    } else if elapsed < 86_400 {
        let hours = elapsed / 3_600;
        format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" })
    } else if elapsed < 7 * 86_400 {
        let days = elapsed / 86_400;
        format!("{days} day{} ago", if days == 1 { "" } else { "s" })
    } else {
        local.format("%b %-d, %H:%M %Z").to_string()
    };
    format!(
        r#"<time datetime="{}" title="{}" data-relative-time>{}</time>"#,
        at.to_rfc3339(),
        e(&format!(
            "{} ({timezone})",
            local.format("%Y-%m-%d %H:%M:%S %Z UTC%:z")
        )),
        e(&label)
    )
}
fn mode(value: DeliveryMode) -> (&'static str, &'static str, &'static str) {
    match value {
        DeliveryMode::Shadow => (
            "shadow",
            "Dry run",
            "The result is recorded in ArtiDezi; nothing is published.",
        ),
        DeliveryMode::MergeRequest => (
            "merge_request",
            "Merge request",
            "Review the proposal before you merge it.",
        ),
        DeliveryMode::MergeRequestWithAutomerge => (
            "merge_request_with_automerge",
            "Merge automatically",
            "Merge the proposal once the required checks pass.",
        ),
        DeliveryMode::DirectPushFastForward => (
            "direct_push_fast_forward",
            "Direct push",
            "Apply the result to the target branch without a review step.",
        ),
    }
}
fn page_heading(title: &str, action: &str) -> String {
    format!(
        r#"<div class="page-heading"><h1>{}</h1>{action}</div>"#,
        e(title)
    )
}

pub(super) fn login(error: Option<&str>) -> String {
    let error = error
        .map(|s| format!(r#"<div class="form-error" role="alert">{}</div>"#, e(s)))
        .unwrap_or_default();
    document(
        "Sign in",
        &format!(
            r#"<main class="login-layout"><section class="login-story">{}<div class="login-message"><h1>The boring buttons.<br><em>Consider them pushed.</em></h1><div class="orbit-art" aria-hidden="true"><div class="orbit orbit-one"></div><div class="orbit orbit-two"></div><div class="orbit-core">a<span>.</span></div><span class="orbit-node node-one">✓</span><span class="orbit-node node-two">↗</span><span class="orbit-node node-three"></span></div></div><p class="login-footnote">ArtiDezi · Your maintenance workspace</p></section><section class="login-panel"><div class="login-card"><span class="login-lock">{}</span><h2>Sign in</h2>{error}<form method="post" action="/login"><label for="username">Username</label><input id="username" name="username" autocomplete="username" required autofocus><label for="password">Password</label><div class="password-wrap"><input id="password" type="password" name="password" autocomplete="current-password" required><button type="button" class="password-toggle" aria-label="Show password">Show</button></div><button class="button primary full-width" type="submit">Sign in</button></form><p class="login-help">{}Sessions expire after 30 days.</p></div></section></main>"#,
            brand(),
            icon("shield"),
            icon("shield")
        ),
    )
}

pub(super) fn error_page(message: &str) -> String {
    document(
        "Unable to complete request",
        &format!(
            r#"<main class="standalone-error">{}<span class="empty-icon">{}</span><h1>Unable to complete request</h1><p>{}</p><a class="button primary" href="/tasks">Back to tasks</a></main>"#,
            brand(),
            icon("alert"),
            e(message)
        ),
    )
}

pub(super) fn account(
    access: &Access,
    token: &Session,
    password_error: Option<&str>,
    timezone_error: Option<&str>,
) -> String {
    let mut body = format!(
        r#"<section class="panel"><header class="card-header"><h1>{}</h1></header><p>Account ID: <code>{}</code></p><p>{}</p>"#,
        e(&access.display_name),
        e(&access.id),
        if access.is_admin() {
            "Instance administrator"
        } else {
            "Project access"
        },
    );
    if !access.is_admin() {
        body.push_str("<ul>");
        for (project, grant) in access.grants() {
            write!(
                body,
                "<li><code>{}</code>: {}</li>",
                e(project),
                if *grant == access::Grant::Write {
                    "Write"
                } else {
                    "Read"
                }
            )
            .unwrap();
        }
        body.push_str("</ul>");
    }
    let timezone_error = timezone_error
        .map(|message| {
            format!(
                r#"<div class="form-error" role="alert">{}</div>"#,
                e(message)
            )
        })
        .unwrap_or_default();
    let mut timezone_options = String::new();
    for zone in std::iter::once(chrono_tz::UTC).chain(
        chrono_tz::TZ_VARIANTS
            .iter()
            .copied()
            .filter(|zone| *zone != chrono_tz::UTC),
    ) {
        let name = zone.to_string();
        write!(
            timezone_options,
            r#"<option value="{}" {}>{}</option>"#,
            e(&name),
            if name == access.timezone {
                "selected"
            } else {
                ""
            },
            e(&name)
        )
        .unwrap();
    }
    write!(body, r#"</section><section class="panel" id="timezone"><header class="card-header"><h2>Time zone</h2></header>{timezone_error}<form method="post" action="/account/timezone"><input type="hidden" name="csrf" value="{}"><label for="account-timezone">Time zone</label><div class="timezone-control"><select id="account-timezone" name="timezone" required>{timezone_options}</select><div class="timezone-helper"><button class="button secondary" id="browser-timezone" type="button" disabled><span>Use browser time zone</span></button></div></div><button class="button primary" type="submit">Save time zone</button></form>"#, e(&token.csrf)).unwrap();
    let error = password_error
        .map(|message| {
            format!(
                r#"<div class="form-error" role="alert">{}</div>"#,
                e(message)
            )
        })
        .unwrap_or_default();
    write!(body, r#"</section><section class="panel" id="change-password"><header class="card-header"><h2>Change password</h2></header>{error}<form class="password-change" method="post" action="/account/password"><input type="hidden" name="csrf" value="{}"><label for="current-password">Current password</label><input id="current-password" name="current_password" type="password" autocomplete="current-password" required><label for="new-password">New password</label><input id="new-password" name="new_password" type="password" autocomplete="new-password" required minlength="16"><label for="confirm-password">Confirm new password</label><input id="confirm-password" name="confirm_password" type="password" autocomplete="new-password" required minlength="16"><button class="button primary" type="submit">Change password</button></form></section>"#, e(&token.csrf)).unwrap();
    shell_with_breadcrumb(
        "Account",
        token,
        "account",
        &body,
        &page_heading("Account", ""),
        true,
    )
}

fn matches_filter(task: &Task, filter: &str) -> bool {
    match filter {
        "active" => task.state == TaskState::Working,
        "waiting" => task.state == TaskState::WaitingForCi,
        "review" => task.state == TaskState::AwaitingReview,
        "attention" => task.state == TaskState::NeedsHuman,
        "done" => task.state == TaskState::Done,
        "closed" => task.state == TaskState::Closed,
        _ => true,
    }
}

fn search_form(path: &str, label: &str, query: &TaskQuery, projects: Option<Vec<&str>>) -> String {
    let mut out = format!(
        r#"<form class="list-search {}" method="get" action="{path}" role="search">"#,
        if projects.is_some() {
            "has-project"
        } else {
            ""
        }
    );
    if let Some(projects) = projects {
        out.push_str(r#"<select id="project-filter" name="project" aria-label="Filter by project"><option value="">All projects</option>"#);
        for name in projects {
            write!(
                out,
                r#"<option value="{}" {}>{}</option>"#,
                e(name),
                if name == query.project {
                    "selected"
                } else {
                    ""
                },
                e(name)
            )
            .unwrap();
        }
        out.push_str("</select>");
    }
    write!(out, r#"<label class="search">{}<input type="search" name="q" value="{}" placeholder="{label}" aria-label="{label}"></label><input type="hidden" name="filter" value="{}"><button class="button secondary" type="submit">Search</button></form><p id="search-error" class="form-error" role="alert" hidden>Search failed. Try again.</p>"#, icon("search"), e(&query.q), e(&query.filter)).unwrap();
    out
}
fn tasks_url(query: &TaskQuery, page: usize) -> String {
    format!(
        "/tasks?project={}&filter={}&q={}&page={page}",
        url(&query.project),
        url(&query.filter),
        url(&query.q)
    )
}

const TASK_PAGE_SIZE: usize = 50;

pub(super) fn task_page<'a>(tasks: &'a [Task], query: &TaskQuery) -> Vec<&'a Task> {
    let search = query.q.trim().to_lowercase();
    let filtered: Vec<_> = tasks
        .iter()
        .filter(|task| {
            (query.project.is_empty() || task.project == query.project)
                && format!(
                    "{} {} {} #{}",
                    task.instructions, task.project, task.id, task.number
                )
                .to_lowercase()
                .contains(&search)
                && matches_filter(task, &query.filter)
        })
        .collect();
    let pages = filtered.len().div_ceil(TASK_PAGE_SIZE).max(1);
    let page = query.page.clamp(1, pages);
    filtered
        .into_iter()
        .skip((page - 1) * TASK_PAGE_SIZE)
        .take(TASK_PAGE_SIZE)
        .collect()
}

pub(super) fn tasks(
    config: &Config,
    tasks: &[Task],
    usage: &std::collections::BTreeMap<String, artidezi_contract::attempt::Usage>,
    token: &Session,
    query: &TaskQuery,
    can_create: bool,
) -> String {
    let action = if can_create {
        format!(
            r#"<a class="button primary" href="/tasks/new">{}New task</a>"#,
            icon("plus")
        )
    } else {
        String::new()
    };
    let mut body = String::new();
    let names: std::collections::BTreeSet<_> = config
        .projects
        .keys()
        .map(String::as_str)
        .chain(tasks.iter().map(|task| task.project.as_str()))
        .chain((!query.project.is_empty()).then_some(query.project.as_str()))
        .collect();
    body.push_str(&search_form(
        "/tasks",
        "Search tasks",
        query,
        Some(names.into_iter().collect()),
    ));
    body.push_str(r#"<section class="task-board" id="live-content" data-refresh="true"><form id="task-filters" class="tabs" method="get" action="/tasks" aria-label="Filter tasks">"#);
    write!(
        body,
        r#"<input type="hidden" name="q" value="{}"><input type="hidden" name="project" value="{}">"#,
        e(&query.q), e(&query.project)
    )
    .unwrap();
    let search = query.q.trim().to_lowercase();
    let matching: Vec<_> = tasks
        .iter()
        .filter(|task| {
            (query.project.is_empty() || task.project == query.project)
                && format!(
                    "{} {} {} #{}",
                    task.instructions, task.project, task.id, task.number
                )
                .to_lowercase()
                .contains(&search)
        })
        .collect();
    for (key, label, symbol) in [
        ("", "All tasks", "tasks"),
        ("active", "In progress", status_icon(TaskState::Working)),
        (
            "waiting",
            "Waiting for CI",
            status_icon(TaskState::WaitingForCi),
        ),
        (
            "review",
            "Ready for review",
            status_icon(TaskState::AwaitingReview),
        ),
        (
            "attention",
            "Needs attention",
            status_icon(TaskState::NeedsHuman),
        ),
        ("done", "Completed", status_icon(TaskState::Done)),
        ("closed", "Closed", status_icon(TaskState::Closed)),
    ] {
        let count = matching
            .iter()
            .filter(|task| matches_filter(task, key))
            .count();
        let tone = if key.is_empty() { "all" } else { key };
        write!(body, r#"<button type="submit" name="filter" value="{key}" class="filter-tab {}" aria-pressed="{}">{}{label}<span class="filter-count {} {}">{count}</span></button>"#,
            if query.filter == key { "active" } else { "" }, query.filter == key, icon(symbol),
            if count > 0 { "nonzero" } else { "" }, tone).unwrap();
    }
    body.push_str(r#"</form><div class="task-list">"#);
    let filtered: Vec<_> = matching
        .into_iter()
        .filter(|task| matches_filter(task, &query.filter))
        .collect();
    let pages = filtered.len().div_ceil(TASK_PAGE_SIZE).max(1);
    let page = query.page.clamp(1, pages);
    if filtered.is_empty() {
        let (title, text, link, action) = if tasks.is_empty() && can_create {
            (
                "No tasks yet",
                "Create a task to get started.",
                "/tasks/new",
                "New task",
            )
        } else {
            (
                "No matching tasks",
                "Try another filter or search.",
                "/tasks",
                "Clear filters",
            )
        };
        write!(
            body,
            r#"<div class="empty-state"><h2>{title}</h2><p>{text}</p>{}</div>"#,
            if tasks.is_empty() && !can_create {
                String::new()
            } else {
                format!(r#"<a class="button secondary" href="{link}">{action}</a>"#)
            }
        )
        .unwrap();
    } else {
        for task in filtered
            .iter()
            .skip((page - 1) * TASK_PAGE_SIZE)
            .take(TASK_PAGE_SIZE)
        {
            let tokens = usage.get(&task.id).map_or_else(String::new, |count| format!(r#"<span class="task-usage"><span class="usage" title="Total input across recorded attempts: {} tokens">↑ {} tok.</span><span class="usage" title="Total output across recorded attempts: {} tokens">↓ {} tok.</span></span>"#, count.input_tokens, compact_tokens(count.input_tokens), count.output_tokens, compact_tokens(count.output_tokens)));
            write!(body, r#"<div class="task-row row-tone-{}"><a class="task-number task-link" href="/tasks/{}">#{}</a><div class="task-row-main"><h3 class="task-heading"><span class="task-origin-slot">{}</span><a class="task-link task-title-link" href="/tasks/{}">{}</a></h3><div class="task-meta"><a class="task-project" href="/tasks?project={}" title="Show tasks for this project">{}{}</a><span aria-hidden="true">·</span>{} {tokens}</div></div><div class="task-row-end">{}</div></div>"#,
                task.state.as_str(), task.number, task.number, task_origin(task, None, false), task.number, e(&task_title(task)), url(&task.project), icon("project"), e(&task.project), time(&task.created_at, &token.access.timezone), status(task)).unwrap();
        }
    }
    body.push_str("</div>");
    if pages > 1 {
        write!(body, r#"<nav class="pagination" aria-label="Task pages"><span>Page {page} of {pages} · {} tasks</span><div>"#, filtered.len()).unwrap();
        if page > 1 {
            write!(
                body,
                r#"<a class="button secondary" rel="prev" href="{}">Previous</a>"#,
                e(&tasks_url(query, page - 1))
            )
            .unwrap();
        }
        if page < pages {
            write!(
                body,
                r#"<a class="button secondary" rel="next" href="{}">Next</a>"#,
                e(&tasks_url(query, page + 1))
            )
            .unwrap();
        }
        body.push_str("</div></nav>");
    }
    body.push_str("</section>");
    root_shell("Tasks", token, "tasks", &action, &body)
}

pub(super) fn new_task(
    config: &Config,
    token: &Session,
    form: &CreateForm,
    error: Option<&str>,
) -> String {
    if config
        .projects
        .get(&form.project)
        .is_some_and(|p| !config.manual_tasks_available(p))
        || !config
            .projects
            .values()
            .any(|p| config.manual_tasks_available(p))
    {
        let body = format!(
            r#"<section class="panel"><header class="card-header"><h1>Incoming changes only</h1></header><p>GitHub projects currently support dry-run evaluations of incoming changes.</p><a class="button primary" href="/incoming-changes?project={}">{}Incoming changes</a></section>"#,
            url(&form.project),
            icon("incoming")
        );
        return shell("New task", config, token, "tasks", &body);
    }
    let selected = config
        .projects
        .get(&form.project)
        .filter(|p| config.manual_tasks_available(p))
        .map(|_| form.project.as_str())
        .or_else(|| {
            config
                .projects
                .iter()
                .find(|(_, p)| config.manual_tasks_available(p))
                .map(|(name, _)| name.as_str())
        })
        .unwrap_or("");
    let mut body = String::new();
    body.push_str(&page_heading("New task", ""));

    if let Some(error) = error {
        write!(
            body,
            r#"<div class="form-error" role="alert">{}</div>"#,
            e(error)
        )
        .unwrap();
    }
    write!(body, r#"<form method="post" action="/tasks" id="create-task" class="create-layout">{}<section class="panel form-card"><header class="card-header"><h2><label for="project">Project</label></h2></header><select name="project" id="project" required>"#, csrf(token)).unwrap();
    for (name, _) in config
        .projects
        .iter()
        .filter(|(_, p)| config.manual_tasks_available(p))
    {
        write!(
            body,
            r#"<option value="{}" {}>{}</option>"#,
            e(name),
            if name == selected { "selected" } else { "" },
            e(name)
        )
        .unwrap();
    }
    write!(
        body,
        r#"</select><p class="project-description" id="project-description">{}</p></section>"#,
        e(config
            .projects
            .get(selected)
            .map(|p| p.description.as_str())
            .unwrap_or(""))
    )
    .unwrap();
    write!(body, r#"<section class="panel form-card"><header class="card-header"><h2><label for="instructions">Task instructions</label></h2></header><p class="field-help">Describe the outcome, useful context and how to check the result.</p><textarea name="instructions" id="instructions" rows="3" maxlength="32000" placeholder="For example: add a hello.sh script, write a small test, and document how to run it." required>{}</textarea><div class="textarea-footer"><span id="character-count">{} / 32,000</span></div></section><section class="panel form-card"><header class="card-header"><h2>Execution</h2></header><div class="form-columns"><div><h3><label for="agent_profile">Agent profile</label></h3><select name="agent_profile" id="agent_profile">"#, e(&form.instructions), form.instructions.chars().count()).unwrap();
    if let Some(project) = config.projects.get(selected) {
        for name in &project.allowed_agent_profiles {
            let chosen = if form.agent_profile.is_empty() {
                &project.default_agent_profile
            } else {
                &form.agent_profile
            };
            write!(
                body,
                r#"<option value="{}" {}>{}{}</option>"#,
                e(name),
                if name == chosen { "selected" } else { "" },
                e(name),
                if name == &project.default_agent_profile {
                    " · default"
                } else {
                    ""
                }
            )
            .unwrap();
        }
    }
    body.push_str(r#"</select></div><fieldset class="delivery-field"><legend>Delivery</legend><div class="delivery-options" id="delivery-options">"#);
    if let Some(project) = config.projects.get(selected) {
        let chosen = if form.delivery_mode.is_empty() {
            mode(project.default_delivery_mode).0
        } else {
            &form.delivery_mode
        };
        for delivery in [
            DeliveryMode::MergeRequest,
            DeliveryMode::MergeRequestWithAutomerge,
            DeliveryMode::DirectPushFastForward,
        ] {
            let (key, label, note) = mode(delivery);
            let allowed = project.allowed_delivery_modes.contains(&delivery);
            write!(body, r#"<label class="delivery-choice" title="{note}{}"><input type="radio" name="delivery_mode" value="{key}" data-explanation="{note}" required {} {}><span class="delivery-option">{}{label}</span></label>"#, if allowed { "" } else { " Not enabled for this project." }, if allowed && chosen == key { "checked" } else { "" }, if allowed { "" } else { "disabled" }, icon(delivery_icon(delivery))).unwrap();
        }
    }
    body.push_str(r#"</div></fieldset></div></section><div class="form-actions"><a class="button secondary" href="/tasks">Cancel</a><button class="button primary" type="submit">Create task <span>→</span></button></div></form>"#);
    // Data stays in inert HTML attributes; the browser never evaluates project configuration.
    body.push_str(r#"<div id="project-options" hidden>"#);
    for (name, project) in config
        .projects
        .iter()
        .filter(|(_, p)| config.manual_tasks_available(p))
    {
        write!(
            body,
            r#"<div data-project="{}" data-description="{}" data-default-profile="{}" data-default-mode="{}">"#,
            e(name),
            e(&project.description),
            e(&project.default_agent_profile),
            mode(project.default_delivery_mode).0
        )
        .unwrap();
        for p in &project.allowed_agent_profiles {
            write!(body, r#"<span data-profile="{}"></span>"#, e(p)).unwrap();
        }
        for m in &project.allowed_delivery_modes {
            let (key, label, note) = mode(*m);
            write!(
                body,
                r#"<span data-mode="{key}" data-label="{label}" data-note="{note}"></span>"#
            )
            .unwrap();
        }
        body.push_str("</div>");
    }
    body.push_str("</div>");
    shell("New task", config, token, "tasks", &body)
}

pub(super) fn confirm_close(task: &Task, config: &Config, token: &Session) -> String {
    let content = format!(
        r#"<div class="create-layout"><section class="panel"><header class="card-header"><h1>Close this task?</h1></header><p class="close-context">{}</p><p>Further task delivery will stop. Existing PRs and history will be kept.</p><form method="post" action="/tasks/{}/close">{}<input type="hidden" name="confirmed" value="true"><div class="form-actions"><a class="button secondary" href="/tasks/{}">Cancel</a><button class="button danger" type="submit">Close task</button></div></form></section></div>"#,
        e(&short(
            task.instructions.lines().next().unwrap_or("Task"),
            140
        )),
        e(&task.id),
        csrf(token),
        task.number
    );
    shell("Close task", config, token, "tasks", &content)
}

pub(super) fn detail(
    detail: &Detail,
    token: &Session,
    notice_key: &str,
    revision_error: Option<(&str, &str)>,
) -> String {
    let task = &detail.task;
    let selected = detail
        .attempts
        .iter()
        .find(|view| view.attempt.number == detail.selected_attempt);
    let viewing_latest = detail
        .attempts
        .last()
        .is_none_or(|view| view.attempt.number == detail.selected_attempt);
    let title = task_title(task);
    let mut body = format!(
        r#"{}<div class="detail-heading"><h1>{}</h1><div class="detail-subtitle">Created by {} <span>·</span> {}</div></div><div class="detail-main"><section class="panel task-overview" id="task-overview" data-task-id="{task_id}" aria-label="Task details"><div class="overview-row">{}{}<dl><div><dt>Project</dt><dd><a class="text-link" href="/projects?q={}">{}</a></dd></div><div><dt>Agent</dt><dd>{}</dd></div><div><dt>Delivery</dt><dd>{}</dd></div></dl>"#,
        notice(notice_key),
        e(&title),
        e(&task.created_by),
        time(&task.created_at, &token.access.timezone),
        status(task),
        task_origin(task, detail.tracking.as_ref(), true),
        e(&task.project),
        e(&task.project),
        profile_link(&task.agent_profile),
        delivery_badge(task.delivery_mode),
        task_id = e(&task.id)
    );
    if detail.can_write && task.delivery_mode != DeliveryMode::Shadow && !task.state.is_terminal() {
        write!(body, r#"<form class="close-task" method="post" action="/tasks/{}/close" data-confirm="Close this task? Further task delivery will stop. Existing PRs and history will be kept.">{}<button class="button danger" type="submit">Close task</button></form>"#, e(&task.id), csrf(token)).unwrap();
    }
    body.push_str("</div>");
    if let Some(source) = &detail.source {
        let url = if source.url.starts_with("https://") || source.url.starts_with("http://") {
            &source.url
        } else {
            "#"
        };
        write!(body, r#"<div class="source-row"><a class="text-link forge-link" href="{}" target="_blank" rel="noopener noreferrer">{}Source change · {} #{}{}</a><span>Evaluated head <code title="{}">{}</code></span><span>Handler {}</span>"#, e(url), icon("forge"), e(&source.repository_id), source.number, icon("external"), e(source.input.commit.as_str()), e(&short(source.input.commit.as_str(), 12)), e(&source.handler_id)).unwrap();
        if let Some(tracking) = &detail.tracking {
            write!(
                body,
                "<span>{}</span>",
                e(&incoming::source_status(tracking, None))
            )
            .unwrap();
            if let Some(id) = &tracking.last_task_id
                && id != &task.id
            {
                write!(
                    body,
                    r#"<a class="text-link" href="/tasks/{}">Latest evaluation →</a>"#,
                    e(id)
                )
                .unwrap();
            }
        }
        body.push_str("</div>");
    }
    let excerpt = instruction_excerpt(&task.instructions);
    body.push_str(r#"</section><section class="panel instructions-panel" id="task-instructions"><header class="card-header"><h2>Task instructions</h2>"#);
    if excerpt != task.instructions {
        body.push_str(r#"<button class="button secondary" id="toggle-instructions" type="button" aria-controls="instructions-content instructions-preview" aria-expanded="true" hidden>Show less</button>"#);
    }
    body.push_str("</header>");
    if excerpt != task.instructions {
        write!(
            body,
            r#"<div class="markdown instructions-text" id="instructions-preview" hidden>{}</div>"#,
            markdown::render(&excerpt)
        )
        .unwrap();
    }
    write!(
        body,
        r#"<div class="markdown instructions-text" id="instructions-content">{}</div>"#,
        markdown::render(&task.instructions)
    )
    .unwrap();
    write!(
        body,
        r#"</section><div id="live-content" data-refresh="{}">"#,
        !task.state.is_terminal()
    )
    .unwrap();
    if task.state == TaskState::NeedsHuman
        && let Some(view) = detail.attempts.last()
    {
        let message = match view.attempt.state {
            AttemptState::Lost => Some((
                "Worker did not finish",
                "The worker stopped responding before it uploaded a result. No assessment was produced. Check the worker health and logs, then evaluate again.".to_owned(),
            )),
            AttemptState::Invalid => Some((
                "Attempt could not be accepted",
                "ArtiDezi rejected the worker result before it could be used. Inspect the attempt details and evaluate again after addressing the reported problem.".to_owned(),
            )),
            _ => detail
                .events
                .iter()
                .rev()
                .find(|event| event.kind == "task.needs_human")
                .and_then(|event| event.payload.get("reason").and_then(|value| value.as_str()))
                .map(|reason| (
                    "Delivery needs attention",
                    if reason == "Automatic delivery requires an unchanged low-risk assessment with a rationale and evidence" {
                        let details = view.result.as_ref().map(|result| {
                            let risk = result.risk_hint.as_ref().map_or("missing", |hint| match hint.level {
                                artidezi_contract::result::RiskLevel::Low => "low",
                                artidezi_contract::result::RiskLevel::Medium => "medium",
                                artidezi_contract::result::RiskLevel::High => "high",
                            });
                            let rationale = result.risk_hint.as_ref().map(|hint| hint.rationale.trim()).filter(|value| !value.is_empty());
                            let evidence = result.evidence.len();
                            format!(
                                " Automatic merge requires a low-risk assessment. This result was rated {risk} risk, with {evidence} evidence item{}.{}",
                                if evidence == 1 { "" } else { "s" },
                                rationale.map_or(String::new(), |value| format!(" Rationale: {value}"))
                            )
                        }).unwrap_or_default();
                        format!("Automatic merge was not attempted.{details}")
                    } else if reason == "could not apply accepted attempt outcome: target or context branch changed; evaluate again" {
                        "The assessment was valid, but the source or target branch changed before ArtiDezi could publish it. Evaluate again to assess the current revision.".to_owned()
                    } else {
                        format!("ArtiDezi could not complete delivery: {reason}")
                    },
                )),
        };
        if let Some((heading, text)) = message {
            write!(
                body,
                r#"<section class="task-attention" role="alert"><h2>{heading}</h2><p>{}</p>{}</section>"#,
                e(&text),
                if detail.can_write && matches!(view.attempt.state, AttemptState::Lost | AttemptState::Invalid)
                    && detail.tracking.as_ref().is_none_or(|source| source.last_task_id.as_deref() == Some(&task.id))
                    && !detail.merge_requests.iter().any(|mr| mr.state == MergeRequestState::Open) {
                    format!(
                        r#"<form method="post" action="/tasks/{}/revisions"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="feedback" value="Retry the evaluation after the previous attempt could not produce an accepted result. Address its recorded failure and preserve the authorized scope."><button class="button primary">Retry evaluation</button></form>"#,
                        e(&task.id),
                        e(&token.csrf),
                    )
                } else {
                    String::new()
                }
            )
            .unwrap();
        }
    }
    if task.state == TaskState::Closed
        && detail
            .events
            .iter()
            .any(|event| event.kind == "assessment.superseded")
        && let Some(tracking) = &detail.tracking
        && tracking.follow_up.is_some()
        && !tracking.paused
    {
        let change = if detail
            .events
            .iter()
            .any(|event| event.kind == "merge_request.superseded")
        {
            "The source changed after its adaptation was published. ArtiDezi closed the obsolete proposal, retired this assessment"
        } else {
            "The source changed before publication. ArtiDezi retired this stale assessment"
        };
        write!(body, r#"<section class="task-attention"><h2>Fresh evaluation scheduled automatically</h2><p>{change} and will assess the new revision after it settles. No action is needed.</p></section>"#).unwrap();
    }
    if let Some((feedback, error)) = revision_error {
        write!(
            body,
            r#"<div class="form-error" role="alert">{}</div>"#,
            e(error)
        )
        .unwrap();
        if task.state.is_terminal() {
            write!(body, r#"<section class="panel"><header class="card-header"><h2>Your feedback</h2></header><p class="prewrap">{}</p></section>"#, e(feedback)).unwrap();
        }
    }
    if !detail.merge_requests.is_empty() {
        body.push_str(r#"<section class="panel proposals"><header class="card-header"><h2>Published changes</h2></header>"#);
        for mr in &detail.merge_requests {
            let link = if mr.url.starts_with("https://") || mr.url.starts_with("http://") {
                mr.url.as_str()
            } else {
                "#"
            };
            write!(body, r#"<a class="proposal forge-link" href="{}" target="_blank" rel="noopener noreferrer"><span class="proposal-icon">{}</span><div><strong>{} <span>#{}</span></strong><p>{} · {}</p></div>{}</a>"#, e(link), icon("forge"), e(&mr.repository_id), mr.number, e(&mr.source_branch), mr.state.as_str(), icon("external")).unwrap();
        }
        body.push_str("</section>");
    }
    body.push_str(r#"<div id="results">"#);
    if detail.attempts.len() > 1 {
        body.push_str(r#"<nav class="attempt-navigation" aria-label="Result attempts">"#);
        for view in detail.attempts.iter().rev() {
            write!(
                body,
                r##"<a class="button {}" href="/tasks/{}?attempt={}#results" {}>Attempt {}</a>"##,
                if view.attempt.number == detail.selected_attempt {
                    "primary"
                } else {
                    "secondary"
                },
                task.number,
                view.attempt.number,
                if view.attempt.number == detail.selected_attempt {
                    r#"aria-current="page""#
                } else {
                    ""
                },
                view.attempt.number
            )
            .unwrap();
        }
        body.push_str("</nav>");
    }
    if let Some(view) = selected {
        body.push_str(&attempt::card(task, view, &token.access.timezone));
    }
    body.push_str("</div></div><div id=\"task-actions\">");
    if !viewing_latest {
        write!(
            body,
            r#"<a class="button secondary" href="/tasks/{}">Return to the latest attempt {}</a>"#,
            task.number,
            icon("arrow")
        )
        .unwrap();
    } else if detail.can_write && task.kind == artidezi_store::model::TaskKind::PullRequest {
        if task.delivery_mode == DeliveryMode::MergeRequest
            && task.state == TaskState::NeedsHuman
            && detail.attempts.last().is_some_and(|a| {
                a.attempt.state == artidezi_store::model::AttemptState::Valid
                    && matches!(
                        a.attempt.outcome,
                        Some(
                            artidezi_contract::common::Outcome::Proposal
                                | artidezi_contract::common::Outcome::ApproveUntouched
                        )
                    )
            })
        {
            write!(body, r#"<section class="panel"><header class="card-header"><h2>Publication</h2></header><p>Retry delivery using the validated result. Existing publication is checked first.</p><form method="post" action="/tasks/{}/retry-publication">{}<button class="button primary">Retry publication</button></form></section>"#, task.number, csrf(token)).unwrap();
        }
        if detail
            .merge_requests
            .iter()
            .any(|mr| mr.state.as_str() == "open")
            && detail.attempts.last().is_some_and(|a| {
                a.attempt.state == artidezi_store::model::AttemptState::Valid
                    && a.attempt.outcome == Some(artidezi_contract::common::Outcome::Proposal)
            })
        {
            write!(body, r#"<section class="panel revision-panel"><header class="card-header"><h2>Revise proposal</h2></header><p>Keeps the existing adaptation PR open and asks a fresh agent to apply your feedback.</p><form method="post" action="/tasks/{}/proposal-revisions">{}<label for="proposal-feedback">Feedback</label><textarea id="proposal-feedback" name="feedback" rows="3" maxlength="32000" placeholder="Describe what should change in the existing proposal." required></textarea><button class="button primary">Revise proposal</button></form></section>"#, task.number, csrf(token)).unwrap();
        }
        if let Some(tracking) = &detail.tracking {
            let is_latest = tracking.last_task_id.as_deref() == Some(&task.id);
            let can_revise = is_latest
                && task.state != TaskState::WaitingForCi
                && detail
                    .attempts
                    .last()
                    .is_none_or(|v| v.attempt.state.is_terminal());
            if can_revise {
                write!(body, r#"<section class="panel revision-panel"><header class="card-header"><h2>Evaluate again</h2></header><p>Uses the current source and rule settings. Resolve open adaptation PRs first.</p><form method="post" action="/tasks/{}/revisions">{}<label for="feedback">Follow-up instructions</label><textarea id="feedback" name="feedback" rows="3" maxlength="32000" required>{}</textarea><button class="button primary">Evaluate again</button></form></section>"#, e(&task.id), csrf(token), e(revision_error.map(|(feedback, _)| feedback).unwrap_or(""))).unwrap();
            }
        }
    } else if detail.can_write && !task.state.is_terminal() {
        let can_revise = detail
            .attempts
            .last()
            .is_none_or(|v| v.attempt.state.is_terminal());
        write!(body, r#"<section class="panel revision-panel"><header class="card-header"><h2>Request a revision</h2></header><p id="revision-context">{}<form method="post" action="/tasks/{}/revisions">{}<label for="feedback">Feedback</label><textarea id="feedback" name="feedback" rows="3" maxlength="32000" placeholder="Describe what to change or clarify." aria-describedby="revision-context" required {}>{}</textarea><button class="button primary" {}>Request revision</button></form></section>"#, if can_revise { "Unhappy with the result? A fresh agent can retry using the existing work and your feedback below.</p>" } else { "Available when the current attempt finishes.</p>" }, e(&task.id), csrf(token), if can_revise { "" } else { "disabled" }, e(revision_error.map(|(feedback, _)| feedback).unwrap_or("")), if can_revise { "" } else { "disabled" }).unwrap();
    }
    body.push_str("</div>");
    body.push_str(r#"<section class="panel activity" id="activity"><header class="card-header"><h2>Activity</h2></header><ol>"#);
    for event in detail.events.iter().rev().take(50) {
        let label = match event.kind.as_str() {
            "attempt.recovery_queued" => "Replacement queued after worker loss".into(),
            "attempt.transient_retry_queued" => {
                "Automatic retry scheduled after a temporary model failure".into()
            }
            "publication.planned" => "Publication prepared".into(),
            "publication.ci_passed" => "CI passed; continuing delivery".into(),
            "publication.retry_requested" => "Publication retry requested".into(),
            "assessment.published" if event.payload["state"] == "approved" => {
                "Source approved".into()
            }
            "assessment.published" => "Assessment review published".into(),
            "assessment.approval_planned" => "Source approval prepared".into(),
            "source.merge_planned" => "Source merge prepared".into(),
            "source.merge_accepted" => "GitHub accepted the source merge".into(),
            "source.merge_observed" => "Source confirmed merged".into(),
            "task.state_changed" => match event.payload.get("to").and_then(|v| v.as_str()) {
                Some("working") if event.payload["from"] == "waiting_for_ci" => {
                    "Continuing delivery".into()
                }
                Some("working") => "Task queued for work".into(),
                Some("awaiting_review") => "Proposal ready for review".into(),
                Some("waiting_for_ci") => {
                    "Waiting for CI; delivery will continue automatically".into()
                }
                Some("needs_human") => "Your input is needed".into(),
                Some("done") if task.delivery_mode == DeliveryMode::Shadow => {
                    "Evaluation recorded without publication".into()
                }
                Some("done") => "Changes delivered".into(),
                Some("closed") => "Task closed".into(),
                _ => "Task status updated".into(),
            },
            "attempt.state_changed" => format!(
                "Attempt {}",
                event
                    .payload
                    .get("to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("updated")
            ),
            "merge_request.published" => "Proposal published to the forge".into(),
            "merge_request.superseded" => "Obsolete proposal closed on the forge".into(),
            "attempt.leased" => "Worker started the attempt".into(),
            "task.created" => "Task created".into(),
            "attempt.created" => "Attempt queued".into(),
            "task.needs_human" => "Feedback requested".into(),
            "attempt.rejected" => "Result rejected".into(),
            "attempt.lost" => "Worker disconnected".into(),
            _ => event.kind.replace(['.', '_'], " "),
        };
        let state = event
            .payload
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (tone, symbol) = match (event.kind.as_str(), state) {
            (_, "needs_human" | "invalid" | "lost")
            | ("task.needs_human" | "attempt.rejected" | "attempt.lost", _) => {
                ("attention", "alert")
            }
            (_, "done" | "valid")
            | ("assessment.recorded" | "merge_request.published" | "publication.ci_passed", _) => {
                ("success", "check")
            }
            (_, "awaiting_review") => ("attention", "eye"),
            ("attempt.leased", _) | (_, "working" | "waiting_for_ci" | "leased" | "queued") => {
                ("progress", "clock")
            }
            _ => ("neutral", "tasks"),
        };
        write!(body, r##"<li class="event-{tone}" id="event-{}"><span class="timeline-icon">{}</span><div><a class="event-link" href="/tasks/{}/events/{}" title="View event details"><strong>{}</strong></a><span>{} · {}</span></div></li>"##, event.id, icon(symbol), task.number, event.id, e(&label), e(&event.actor), time(&event.at, &token.access.timezone)).unwrap();
    }
    body.push_str("</ol></section></div>");
    task_shell(task, token, None, &body)
}

pub(super) fn workers(
    config: &Config,
    listed_workers: &std::collections::BTreeMap<String, artidezi_config::model::WorkerPool>,
    attempts: &[Attempt],
    workers: &[Worker],
    lease_ttl_seconds: u64,
    discovery_paused: bool,
    token: &Session,
) -> String {
    let mut body = String::from(
        r#"<p class="muted">Configured worker pools and queue activity. Draining stops new assignments and lets current work finish.</p><div class="project-list">"#,
    );
    if token.access.is_admin() {
        write!(body, r#"<article class="panel"><header class="card-header"><h2>Maintenance scheduling</h2></header><p>{}</p><p>Pausing defers automatic discovery and forge maintenance. Current worker results can still finish publishing. Pair this with worker drain before an upgrade.</p><form method="post" action="/workers/maintenance"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="discovery_paused" value="{}"><button class="button" type="submit">{}</button></form></article>"#,
            if discovery_paused { "Automatic scheduling is paused." } else { "Automatic scheduling is running." }, e(&token.csrf), !discovery_paused,
            if discovery_paused { "Resume automatic scheduling" } else { "Pause automatic scheduling" }).unwrap();
    }
    for (name, pool) in &config.worker_pools {
        let queued = attempts
            .iter()
            .filter(|a| a.worker_pool == *name && a.state == AttemptState::Queued)
            .count();
        let active = attempts
            .iter()
            .filter(|a| a.worker_pool == *name && a.state == AttemptState::Leased)
            .count();
        let completed = attempts
            .iter()
            .filter(|a| a.worker_pool == *name && a.state.is_terminal())
            .count();
        let harnesses = pool
            .harnesses
            .iter()
            .map(|h| format!("{h:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ");
        let sandbox = match pool.harness_sandbox {
            artidezi_contract::job::HarnessSandbox::Disabled => "Disabled",
            artidezi_contract::job::HarnessSandbox::Strict => "Strict",
        };
        write!(
            body,
            r#"<article class="panel project-card worker-card"><header class="card-header"><div><h2>{}</h2><p>Worker pool</p></div><span class="status {}">{}{}</span></header><dl class="profile-facts"><div><dt>Queued attempts</dt><dd>{queued}</dd></div><div><dt>Active attempts</dt><dd>{active}</dd></div><div><dt>Completed attempts</dt><dd>{completed}</dd></div><div><dt>Harnesses</dt><dd>{}</dd></div><div><dt>Sandbox</dt><dd>{sandbox}</dd></div></dl></article>"#,
            e(name),
            if active > 0 { "progress" } else { "neutral" },
            icon(if active > 0 { "clock" } else { "check" }),
            if active > 0 { "Active" } else { "Idle" },
            e(&harnesses)
        )
        .unwrap();
    }
    if config.worker_pools.is_empty() {
        body.push_str(
            r#"<div class="panel empty-state"><h2>No worker pools configured</h2></div>"#,
        );
    }
    body.push_str("</div>");
    if token.access.is_admin() {
        body.push_str(r#"<h2>Individual workers</h2><p class="muted">Last contact reflects supervisor requests. It does not prove that a running command is making progress. Refresh this page for current status.</p><div class="project-list">"#);
        let now = chrono::Utc::now();
        for worker in workers {
            let offline = (now - worker.last_seen).num_seconds() > lease_ttl_seconds as i64;
            let cooling = worker.cooldown_until.is_some_and(|until| until > now);
            let (label, tone) = if worker.quarantined {
                ("Quarantined", "failed")
            } else if offline {
                ("Offline", "failed")
            } else if worker.drained {
                (
                    if worker.attempt_id.is_some() {
                        "Draining"
                    } else {
                        "Drained"
                    },
                    "neutral",
                )
            } else if cooling {
                ("Cooling down", "neutral")
            } else if worker.attempt_id.is_some() {
                ("Running", "progress")
            } else {
                ("Idle", "neutral")
            };
            let task = worker.task_number.map_or_else(
                || "None".into(),
                |number| format!(r#"<a href="/tasks/{number}">Task #{number}</a>"#),
            );
            let expiry = worker
                .lease_expires_at
                .map_or_else(|| "None".into(), |at| time(&at, &token.access.timezone));
            let cooldown = worker
                .cooldown_until
                .filter(|at| *at > now)
                .map_or_else(|| "None".into(), |at| time(&at, &token.access.timezone));
            let tags = if worker.tags.is_empty() {
                "None".to_owned()
            } else {
                tag_badges(&worker.tags)
            };
            write!(body, r#"<article class="panel project-card individual-worker"><header class="card-header"><div><h2>{}</h2><p>Pool: {}</p></div><span class="status {tone}">{label}</span></header><dl class="profile-facts"><div><dt>Last contact</dt><dd>{}</dd></div><div><dt>Tags</dt><dd>{tags}</dd></div><div><dt>Current work</dt><dd>{task}</dd></div><div><dt>Lease expires</dt><dd>{expiry}</dd></div><div><dt>Cooldown ends</dt><dd>{cooldown}</dd></div></dl><form method="post" action="/workers/drain"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="worker_pool" value="{}"><input type="hidden" name="worker_id" value="{}"><input type="hidden" name="drained" value="{}"><button class="button" type="submit">{}</button></form></article>"#,
                e(&worker.worker_id), e(&worker.worker_pool), time(&worker.last_seen, &token.access.timezone),
                e(&token.csrf), e(&worker.worker_pool), e(&worker.worker_id), !worker.drained,
                if worker.drained { "Resume assignments" } else { "Drain worker" }).unwrap();
        }
        for (pool, definition) in listed_workers {
            for id in definition.workers.keys() {
                if !workers
                    .iter()
                    .any(|worker| worker.worker_pool == *pool && worker.worker_id == *id)
                {
                    write!(body, r#"<article class="panel project-card individual-worker"><header class="card-header"><div><h2>{}</h2><p>Pool: {}</p></div><span class="status neutral">Never connected</span></header></article>"#, e(id), e(pool)).unwrap();
                }
            }
        }
        body.push_str("</div>");
    }
    root_shell("Workers", token, "workers", "", &body)
}

pub(super) fn projects(
    config: &Config,
    sources: &[TrackedPullRequest],
    tasks: &[Task],
    token: &Session,
    query: &TaskQuery,
    access: &Access,
) -> String {
    let mut body = String::new();
    body.push_str(&search_form(
        "/projects",
        "Search projects or repositories",
        query,
        None,
    ));
    let search = query.q.trim().to_lowercase();
    let projects: Vec<_> = config
        .projects
        .iter()
        .filter(|(name, project)| {
            let repositories = project
                .repositories
                .values()
                .map(|r| r.remote.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            format!("{name} {} {repositories}", project.description)
                .to_lowercase()
                .contains(&search)
        })
        .collect();
    write!(body, r#"<div id="live-content" data-refresh="false"><p class="results-count" role="status">{} of {} projects</p><div class="project-list">"#, projects.len(), config.projects.len()).unwrap();
    if projects.is_empty() {
        body.push_str(r#"<div class="panel empty-state"><h2>No matching projects</h2><p>Try another name or repository.</p></div>"#);
    }
    for (name, project) in projects {
        let tracked: Vec<_> = sources.iter().filter(|s| s.project == *name).collect();
        let held = tracked
            .iter()
            .filter(|s| s.paused && !source_is_closed(s))
            .count();
        let count = project.repositories.len();
        let manual = config.manual_tasks_available(project);
        let action = if manual && access.can_write(name) {
            format!(
                r#"<a class="button primary" href="/tasks/new?project={}">{}New task</a>"#,
                e(name),
                icon("plus")
            )
        } else {
            String::new()
        };
        let mut guidance = Vec::new();
        write!(body, r#"<article class="panel project-card"><header class="card-header project-heading"><div><h2>{}</h2><p>{}</p></div>{action}</header><div class="project-meta"><span class="badge repository-count {}">{count} {}</span>{}"#, e(name), e(&project.description), if count == 1 { "single" } else { "multiple" }, if count == 1 { "repository" } else { "repositories" }, if manual { delivery_badge(project.default_delivery_mode) } else { String::new() }).unwrap();
        let task_count = tasks.iter().filter(|task| task.project == *name).count();
        write!(body, r#"<a class="button secondary project-tasks" href="/tasks?project={}">{}{task_count} {}</a>{}"#, url(name), icon("tasks"), if task_count == 1 { "task" } else { "tasks" }, profile_link(&project.default_agent_profile)).unwrap();
        body.push_str(&tag_badges(&project.required_worker_tags));
        if let Some(discovery) = &project.pull_requests {
            for handler in discovery
                .handlers
                .iter()
                .filter(|h| h.action == artidezi_config::model::PullRequestAction::Assess)
            {
                let merges_adaptations = handler.adaptation_action
                    == artidezi_config::model::AdaptationAction::Merge
                    && project.effects.merge_adaptation_merge_requests;
                let (label, note) = match handler.mode {
                    artidezi_config::model::PullRequestMode::DryRun => {
                        ("Dry run", "Results stay in ArtiDezi".to_owned())
                    }
                    artidezi_config::model::PullRequestMode::Publish => {
                        let adaptations = if merges_adaptations {
                            "adaptations merge once their checks pass, unless a reviewer requests changes"
                        } else {
                            "adaptations require review"
                        };
                        match handler.safe_verdict_action {
                            artidezi_config::model::SafeVerdictAction::Comment => (
                                "Publish",
                                format!(
                                    "Publishes comment reviews and adaptation PRs; {adaptations}"
                                ),
                            ),
                            artidezi_config::model::SafeVerdictAction::Approve => (
                                "Approve safe updates",
                                format!(
                                    "Approves unchanged updates with successful checks; {adaptations}"
                                ),
                            ),
                            artidezi_config::model::SafeVerdictAction::Merge => (
                                "Merge safe updates",
                                format!(
                                    "Approves and merges unchanged updates from the handler's authors with successful checks; {adaptations}"
                                ),
                            ),
                        }
                    }
                };
                write!(
                    body,
                    r#"<span class="badge" title="{}">{} · {label}</span>"#,
                    e(&note),
                    e(&handler.id)
                )
                .unwrap();
                guidance.push(format!(
                    r#"<details class="project-guidance"><summary>Assessment guidance · {}</summary><div class="markdown">{}</div></details>"#,
                    e(&handler.id),
                    markdown::render(&handler.instructions)
                ));
            }
        }
        if project.pull_requests.is_some() {
            let open = tracked
                .iter()
                .filter(|source| !source_is_closed(source))
                .count();
            write!(body, r#"<a class="button secondary discovery-link" href="/incoming-changes?project={}" title="Open incoming changes tracked for this project">{}{open} incoming {}</a>"#, url(name), icon("incoming"), if open == 1 { "change" } else { "changes" }).unwrap();
            if held > 0 {
                write!(body, r#"<span class="status needs_human" title="Automatic evaluations are paused for these changes.">{held} paused</span>"#).unwrap();
            }
        }
        body.push_str("</div>");
        for item in guidance {
            body.push_str(&item);
        }
        body.push_str(r#"<div class="repo-list" aria-label="Repositories">"#);
        for (id, repo) in &project.repositories {
            let link = repository_url(config, &repo.forge, &repo.remote);
            let (access, label, symbol, note) = match repo.agent_access {
                artidezi_contract::common::RepositoryAccess::ReadOnly => (
                    "read-only",
                    "Read only",
                    "eye",
                    "Context only; changes to this repository are not accepted.",
                ),
                artidezi_contract::common::RepositoryAccess::ReadWrite => (
                    "read-write",
                    "Changes allowed",
                    "edit",
                    "The agent may propose changes to this repository.",
                ),
            };
            write!(body, r#"<div class="repository-row"><a class="repository-link forge-link" href="{}" target="_blank" rel="noopener noreferrer">{}<span><strong>{}</strong><span>{}</span></span>{}</a><div class="repository-badges"><span class="badge repository-branch" title="Target branch">{}{}</span><span class="badge repository-access {access}" title="{note}">{}{label}</span></div></div>"#, e(&link), icon("forge"), e(id), e(&repo.remote), icon("external"), icon("branch"), e(&repo.branch), icon(symbol)).unwrap();
        }
        body.push_str("</div></article>");
    }
    body.push_str("</div></div>");
    root_shell("Projects", token, "projects", "", &body)
}

fn compact_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}m", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

fn execution_facts(record: &AttemptRecord) -> String {
    let seconds = (record.harness.finished_at - record.harness.started_at)
        .num_seconds()
        .max(0);
    let duration = if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    };
    let harness = match record.harness.name.as_str() {
        "fake" => "Scripted",
        "codex" => "Codex",
        "claude" => "Claude",
        other => other,
    };
    let mut out = format!(
        r#"<div class="execution-facts"><span>Worker <strong>{}</strong></span><span title="Harness version: {}">Harness <strong>{}</strong></span><span class="usage" title="Agent runtime">{}{duration}</span>"#,
        e(&record.supervisor.worker_id),
        e(&record.harness.version),
        e(harness),
        icon("clock")
    );
    if let Some(usage) = &record.harness.usage {
        write!(out, r#"<span class="usage" title="Input: {} tokens sent to the agent">↑ {} tok.</span><span class="usage" title="Output: {} tokens generated by the agent">↓ {} tok.</span>"#, usage.input_tokens, compact_tokens(usage.input_tokens), usage.output_tokens, compact_tokens(usage.output_tokens)).unwrap();
    }
    out.push_str("</div>");
    out
}

pub(super) fn agent_profiles(config: &Config, token: &Session) -> String {
    let mut body = String::new();
    body.push_str(r#"<p class="muted">Current configuration. Existing tasks keep the settings they started with.</p>"#);
    for (name, profile) in &config.agent_profiles {
        let harness = match profile.harness {
            artidezi_contract::job::Harness::Codex => "Codex",
            artidezi_contract::job::Harness::Pi => "Pi",
            artidezi_contract::job::Harness::Claude => "Claude",
            artidezi_contract::job::Harness::Fake => "Scripted",
        };
        write!(body, r#"<article class="panel agent-card" id="{}"><header class="card-header"><h2>{}</h2></header><dl class="profile-facts"><div><dt>Model</dt><dd>{}</dd></div><div><dt>Harness</dt><dd title="Version requirement: {}">{harness}</dd></div><div><dt>Worker pool</dt><dd>{}</dd></div>"#, e(name), e(name), e(if profile.harness == artidezi_contract::job::Harness::Fake { "No model (scripted)" } else { &profile.model }), e(&profile.harness_version_constraint), e(&profile.worker_pool)).unwrap();
        let effort = profile
            .model_settings
            .get("model_reasoning_effort")
            .and_then(|value| value.as_str())
            .unwrap_or(
                if profile.harness == artidezi_contract::job::Harness::Fake {
                    "Not applicable"
                } else {
                    "Default"
                },
            );
        write!(
            body,
            "<div><dt>Reasoning effort</dt><dd>{}</dd></div>",
            e(effort)
        )
        .unwrap();
        body.push_str("</dl>");
        let mut default_projects = Vec::new();
        let mut optional_projects = Vec::new();
        for (project, definition) in &config.projects {
            if definition.default_agent_profile == *name {
                default_projects.push(project);
            } else if definition.allowed_agent_profiles.contains(name) {
                optional_projects.push(project);
            }
        }
        for (label, symbol, class, projects) in [
            ("Default for", "star", "default", default_projects),
            ("Also available for", "plus", "optional", optional_projects),
        ] {
            if projects.is_empty() {
                write!(
                    body,
                    r#"<div class="profile-projects"><span class="profile-projects-label {class}">{}{label}:</span><span class="muted">None</span></div>"#,
                    icon(symbol)
                )
                .unwrap();
                continue;
            }
            write!(
                body,
                r#"<div class="profile-projects"><span class="profile-projects-label {class}">{}{label} {} {}:</span>"#,
                icon(symbol),
                projects.len(),
                if projects.len() == 1 {
                    "project"
                } else {
                    "projects"
                }
            )
            .unwrap();
            for project in projects {
                write!(
                    body,
                    r#"<a class="button secondary" href="/projects?q={}">{}{}</a>"#,
                    url(project),
                    icon("project"),
                    e(project)
                )
                .unwrap();
            }
            body.push_str("</div>");
        }
        body.push_str("</article>");
    }
    root_shell("Agent profiles", token, "agents", "", &body)
}

fn instruction_excerpt(instructions: &str) -> String {
    // Bound both long paragraphs and imported PR boilerplate, without hiding text in an accordion.
    let excerpt = instructions.lines().take(12).collect::<Vec<_>>().join("\n");
    if excerpt.chars().count() > 1_600 {
        short(&excerpt, 1_600)
    } else if excerpt != instructions.trim_end_matches('\n') {
        format!("{excerpt}\n…")
    } else {
        instructions.to_string()
    }
}

pub(super) fn event(_config: &Config, task: &Task, event: &Event, token: &Session) -> String {
    let body = format!(
        r##"{}<section class="panel"><dl class="profile-facts"><div><dt>Event</dt><dd>{}</dd></div><div><dt>Actor</dt><dd>{}</dd></div><div><dt>Time</dt><dd>{}</dd></div></dl><pre class="event-payload">{}</pre></section>"##,
        page_heading(
            "Event details",
            &format!(
                r##"<a class="button secondary" href="/tasks/{}#event-{}">Back to task #{}</a>"##,
                task.number, event.id, task.number
            )
        ),
        e(&event.kind),
        e(&event.actor),
        time(&event.at, &token.access.timezone),
        e(&serde_json::to_string_pretty(&event.payload).unwrap_or_default())
    );
    task_shell(task, token, Some("Event"), &body)
}

fn repository_url(config: &Config, forge: &str, remote: &str) -> String {
    let Some(forge) = config.forges.get(forge) else {
        return "#".into();
    };
    let base = forge
        .web_base_url
        .as_deref()
        .unwrap_or(&forge.api_base_url)
        .trim_end_matches('/');
    format!(
        "{base}/{}",
        remote.split('/').map(url).collect::<Vec<_>>().join("/")
    )
}

fn source_is_closed(source: &TrackedPullRequest) -> bool {
    source.closed || source.status == "Source PR is closed or merged"
}

fn source_title<'a>(source: &'a TrackedPullRequest, task: Option<&'a Task>) -> &'a str {
    if !source.title.is_empty() {
        return &source.title;
    }
    // Older tracking records gain a title on their next scan. Their assessment input already
    // contains it; reading it here avoids triggering work just to populate a display field.
    task.and_then(|task| task.instructions.split_once("\n\nSource title:\n"))
        .map(|(_, rest)| {
            rest.split("\n\nSource description:\n")
                .next()
                .unwrap_or(rest)
        })
        .unwrap_or("Incoming change")
}

fn task_title(task: &Task) -> String {
    let source = serde_json::from_str::<serde_json::Value>(&task.policy_snapshot)
        .ok()
        .and_then(|policy| {
            let source = policy.get("source_pull_request")?;
            let repository_id = source.get("repository_id")?.as_str()?;
            let number = source.get("number")?.as_u64()?;
            let remote = policy
                .get("repositories")?
                .get(repository_id)?
                .get("remote")?
                .as_str()?;
            let title = task
                .instructions
                .split_once("\n\nSource title:\n")
                .and_then(|(_, rest)| rest.split("\n\nSource description:").next())
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(|title| short(title, 140));
            Some(match title {
                Some(title) => format!("Evaluate {remote} #{number} ({title})"),
                None => format!("Evaluate {remote} #{number}"),
            })
        });
    source.unwrap_or_else(|| {
        short(
            task.instructions.lines().next().unwrap_or("Untitled task"),
            140,
        )
    })
}

fn task_breadcrumb(task: &Task, leaf: Option<&str>, attempt: Option<&Attempt>) -> String {
    let mut breadcrumb = format!(
        r#"<a href="/tasks">Tasks</a><span aria-hidden="true">/</span><a href="/tasks?project={}">{}</a><span aria-hidden="true">/</span>"#,
        url(&task.project),
        e(&task.project)
    );
    let title = format!("Task #{}", task.number);
    if let Some(leaf) = leaf {
        write!(
            breadcrumb,
            r#"<a href="/tasks/{}">{title}</a><span aria-hidden="true">/</span>"#,
            task.number
        )
        .unwrap();
        if let Some(attempt) = attempt {
            write!(breadcrumb, r##"<a href="/tasks/{}?attempt={}#attempt-{}">Attempt #{}</a><span aria-hidden="true">/</span>"##, task.number, attempt.number, e(&attempt.id), attempt.number).unwrap();
        }
        write!(
            breadcrumb,
            r#"<span aria-current="page">{}</span>"#,
            e(leaf)
        )
        .unwrap();
    } else {
        write!(breadcrumb, r#"<span aria-current="page">{title}</span>"#).unwrap();
    }
    breadcrumb
}

fn task_shell(task: &Task, token: &Session, leaf: Option<&str>, content: &str) -> String {
    shell_with_breadcrumb(
        &format!("Task #{}", task.number),
        token,
        "tasks",
        content,
        &task_breadcrumb(task, leaf, None),
        false,
    )
}

fn root_shell(title: &str, token: &Session, active: &str, action: &str, content: &str) -> String {
    shell_with_breadcrumb(
        title,
        token,
        active,
        content,
        &page_heading(title, action),
        true,
    )
}

pub(super) fn patch(artifact: &TextArtifact, token: &Session) -> String {
    let task = &artifact.task;
    let raw = format!(
        "/tasks/{}/attempts/{}/logs/{}",
        task.number, artifact.attempt.id, artifact.entry.sha256
    );
    let mut body = page_heading(
        &artifact.entry.name,
        &format!(
            r#"<a class="button secondary" href="{}" target="_blank" rel="noopener">{}Raw patch</a>"#,
            e(&raw),
            icon("file")
        ),
    );
    body.push_str(&diff::render(&artifact.bytes));
    shell_with_breadcrumb(
        &artifact.entry.name,
        token,
        "tasks",
        &body,
        &task_breadcrumb(task, Some(&artifact.entry.name), Some(&artifact.attempt)),
        false,
    )
}
