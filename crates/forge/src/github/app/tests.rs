// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::CiLogOutcome;
use crate::github::fixture::{Fixture, Reply, permissions};

fn fixture() -> Fixture {
    Fixture::new(&"b".repeat(40), &"a".repeat(40))
}

#[test]
fn discovery_listing_uses_complete_open_pages_without_pr_details() {
    let fixture = fixture();
    let make_pull = |number| {
        json!({
            "number": number,
            "title": "Update library",
            "user": {"login": "renovate[bot]", "type": "Bot"},
            "state": "open",
            "draft": false,
            "head": {"ref": "renovate/library", "sha": "a".repeat(40), "repo": {"full_name": "a-fork/app"}},
            "base": {"ref": "main", "sha": "b".repeat(40), "repo": {"full_name": "org/app"}}
        })
    };
    {
        let mut state = fixture.state.lock().unwrap();
        for (page, pulls) in [
            (1, (1..=100).map(make_pull).collect::<Vec<_>>()),
            (2, vec![make_pull(101)]),
        ] {
            state.overrides.insert(
                format!(
                    "/repos/org/app/pulls?state=open&sort=created&direction=asc&per_page=100&page={page}"
                ),
                Reply::new(200, json!(pulls)),
            );
        }
    }
    let forge = fixture.forge();
    let listed = forge
        .list_open_merge_request_summaries("org/app", None)
        .unwrap();
    assert_eq!(listed.len(), 101);
    assert!(listed.iter().all(|entry| entry.details.is_none()));
    assert_eq!(listed[0].summary.head_commit.as_str(), "a".repeat(40));
    assert_eq!(listed[100].summary.number, 101);
    let state = fixture.state.lock().unwrap();
    let reads = state
        .requests
        .iter()
        .filter(|request| request.method == "GET" && request.path.starts_with("/repos/org/app"))
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2);
    assert!(reads.iter().all(|request| request.path.contains("/pulls?")));
}

#[test]
fn selected_failed_job_log_uses_actions_only_token_and_never_forwards_it() {
    let fixture = fixture();
    {
        let mut state = fixture.state.lock().unwrap();
        state.actions_permission = true;
        state.conclusion = "failure".into();
    }
    let forge = fixture.forge();
    let logs = forge.ci_job_logs("org/app", &CommitSha("a".repeat(40)), &["tests".into()]);
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].outcome, CiLogOutcome::Available);
    assert_eq!(logs[0].excerpt.as_deref(), Some("setup\nfailed\n"));
    assert_eq!(logs[0].job_id, Some(27));
    assert!(!logs[0].truncated);
    let state = fixture.state.lock().unwrap();
    assert!(
        state
            .requests
            .iter()
            .any(|r| r.body["permissions"] == crate::github::fixture::actions_read_permissions())
    );
    assert!(
        state
            .requests
            .iter()
            .any(|r| r.path == "/signed-job-log" && !r.headers.contains_key("authorization"))
    );
    assert!(
        !state
            .requests
            .iter()
            .any(|r| r.path == "/repos/org/app/actions/jobs/27/logs"
                && !r.headers.contains_key("authorization"))
    );
}

#[test]
fn selected_logs_fail_closed_on_permission_size_and_source_mismatch() {
    let fixture = fixture();
    let forge = fixture.forge();
    let head = CommitSha("a".repeat(40));
    let names = ["tests".into()];
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &names)[0].outcome,
        CiLogOutcome::PermissionDenied
    );
    {
        let mut state = fixture.state.lock().unwrap();
        state.actions_permission = true;
        state.conclusion = "failure".into();
        let mut oversized = Reply::new(200, serde_json::Value::Null);
        oversized.raw = Some(vec![b'x'; 8 * 1024 * 1024 + 1]);
        state.overrides.insert("/signed-job-log".into(), oversized);
    }
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &names)[0].outcome,
        CiLogOutcome::TooLarge
    );
    {
        let mut state = fixture.state.lock().unwrap();
        state.overrides.remove("/signed-job-log");
        state.overrides.insert("/repos/org/app/actions/runs/9".into(), Reply::new(200, serde_json::json!({"id":9,"head_sha":head.as_str(),"run_attempt":2,"repository":{"full_name":"org/app"}})));
    }
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &names)[0].outcome,
        CiLogOutcome::FetchFailed
    );
}

#[test]
fn selected_logs_report_pending_missing_and_refuse_untrusted_redirects() {
    let fixture = fixture();
    fixture.state.lock().unwrap().actions_permission = true;
    let forge = fixture.forge();
    let head = CommitSha("a".repeat(40));
    assert!(
        forge
            .ci_job_logs("org/app", &head, &["other".into()])
            .is_empty()
    );
    fixture.state.lock().unwrap().conclusion = "running".into();
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &["tests".into()])[0].outcome,
        CiLogOutcome::Pending
    );
    {
        let mut state = fixture.state.lock().unwrap();
        state.conclusion = "failure".into();
        state.overrides.insert(
            "/repos/org/app/actions/jobs/27/logs".into(),
            Reply::new(404, serde_json::Value::Null),
        );
    }
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &["tests".into()])[0].outcome,
        CiLogOutcome::MissingOrInaccessible
    );
    {
        let mut state = fixture.state.lock().unwrap();
        let mut redirect = Reply::new(302, serde_json::Value::Null);
        redirect
            .headers
            .insert("location".into(), "https://example.com/log".into());
        state
            .overrides
            .insert("/repos/org/app/actions/jobs/27/logs".into(), redirect);
    }
    assert_eq!(
        forge.ci_job_logs("org/app", &head, &["tests".into()])[0].outcome,
        CiLogOutcome::FetchFailed
    );
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.path == "/log")
    );
}

#[test]
fn automatically_selected_jobs_are_capped_and_deterministic() {
    let fixture = fixture();
    {
        let mut state = fixture.state.lock().unwrap();
        state.actions_permission = true;
        state.conclusion = "failure".into();
        let head = state.head.clone();
        let jobs: Vec<_> = [(29, "zeta"), (27, "tests"), (28, "alpha")]
            .into_iter()
            .map(|(id, name)| {
                serde_json::json!({
                    "id": id, "run_id": 9, "head_sha": head, "name": name,
                    "status": if name == "tests" { "completed" } else { "in_progress" },
                    "conclusion": if name == "tests" { Some("failure") } else { None }
                })
            })
            .collect();
        state.overrides.insert(
            "/repos/org/app/actions/runs/9/attempts/1/jobs?per_page=100&page=1".into(),
            Reply::new(200, serde_json::json!({"total_count": 3, "jobs": jobs})),
        );
    }
    let logs = fixture.forge().ci_job_logs(
        "org/app",
        &CommitSha("a".repeat(40)),
        &["zeta".into(), "alpha".into(), "tests".into()],
    );
    assert_eq!(
        logs.iter().map(|log| log.name.as_str()).collect::<Vec<_>>(),
        vec!["tests", "alpha"]
    );
    assert_eq!(logs[0].outcome, CiLogOutcome::Available);
    assert_eq!(logs[1].outcome, CiLogOutcome::Pending);
}

#[test]
fn signed_auth_scoped_tokens_reads_and_conditional_cache() {
    let fixture = fixture();
    let forge = fixture.forge();
    assert_eq!(forge.verify_installation().unwrap(), vec!["org/app"]);
    let first = forge
        .list_open_merge_requests("org/app", Some("renovate[bot]"))
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].head_commit.as_str(), "a".repeat(40));
    assert_eq!(first[0].checks, ChecksState::Success);
    assert_eq!(
        first[0].check_results,
        Some(vec![CheckResult {
            kind: CheckResultKind::CheckRun,
            name: "tests".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
        }])
    );
    assert_eq!(
        forge.branch_head("org/app", "main").unwrap().as_str(),
        "b".repeat(40)
    );
    assert_eq!(
        forge.merge_request_ref("org/app", 33).unwrap(),
        "refs/pull/33/head"
    );
    assert_eq!(forge.get_merge_request("org/app", 33).unwrap(), first[0]);
    assert_eq!(fixture.state.lock().unwrap().issued, 1);
    assert!(fixture.state.lock().unwrap().not_modified > 0);
    assert!(!forge.writes_allowed());
    let count = fixture.state.lock().unwrap().requests.len();
    let sha = CommitSha("a".repeat(40));
    assert!(
        forge
            .create_merge_request(
                "org/app",
                &NewMergeRequest {
                    title: "x".into(),
                    body: "x".into(),
                    source_branch: "x".into(),
                    target_branch: "main".into(),
                    draft: true
                }
            )
            .is_err()
    );
    assert!(forge.update_merge_request("org/app", 33, "x", "x").is_err());
    assert!(
        forge
            .merge("org/app", 33, &sha, MergeMethod::Merge)
            .is_err()
    );
    assert!(forge.create_tag("org/app", "v1", &sha).is_err());
    assert_eq!(
        fixture.state.lock().unwrap().requests.len(),
        count,
        "write methods must not reach HTTP"
    );
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == "GET"
                || (r.method == "POST" && r.path == "/app/installations/42/access_tokens"))
    );
    let credentials = forge.git_credentials("org/app").unwrap().unwrap();
    assert!(!format!("{credentials:?}").contains("fixture-installation-token"));
}

#[test]
fn expires_tokens_before_the_deadline_and_recovers_from_revocation_without_ambient_credentials() {
    let fixture = fixture();
    let forge = fixture.forge();
    forge.verify_installation().unwrap();
    forge
        .tokens
        .lock()
        .unwrap()
        .get_mut("org/app")
        .unwrap()
        .expires_at = Utc::now() + Duration::seconds(60);
    forge.verify_installation().unwrap();
    assert_eq!(fixture.state.lock().unwrap().issued, 2);
    fixture.state.lock().unwrap().overrides.insert(
        "/repos/org/app".into(),
        Reply::new(
            401,
            json!({"message":"fixture-installation-token-private-body"}),
        ),
    );
    let error = forge.verify_installation().unwrap_err().to_string();
    assert!(!error.contains("private-body"));
    assert!(forge.tokens.lock().unwrap().is_empty());
    fixture.state.lock().unwrap().overrides.clear();
    forge.verify_installation().unwrap();
    assert_eq!(fixture.state.lock().unwrap().issued, 3);
}

#[test]
fn wrong_app_installation_permissions_and_token_scope_are_rejected() {
    let good_installation = json!({"id":42,"app_id":17,"account":{"login":"org"},"repository_selection":"selected","permissions":permissions(),"suspended_at":null});
    let good_token = json!({"token":"fixture-installation-token-bad","expires_at":(Utc::now()+Duration::hours(1)).to_rfc3339(),"permissions":permissions(),"repositories":[{"full_name":"org/app"}]});
    let mut cases = vec![];
    for (field, value) in [
        ("app_id", json!(99)),
        ("account", json!({"login":"other"})),
        ("repository_selection", json!("unknown")),
        ("suspended_at", json!("today")),
    ] {
        let mut v = good_installation.clone();
        v[field] = value;
        cases.push(("/app/installations/42", 200, v));
    }
    let mut missing = good_installation.clone();
    missing["permissions"]
        .as_object_mut()
        .unwrap()
        .remove("checks");
    cases.push(("/app/installations/42", 200, missing));
    for (field, value) in [
        ("repositories", json!([{"full_name":"org/other"}])),
        ("expires_at", json!(Utc::now().to_rfc3339())),
    ] {
        let mut v = good_token.clone();
        v[field] = value;
        cases.push(("/app/installations/42/access_tokens", 201, v));
    }
    cases.push((
        "/app",
        200,
        json!({"id":17,"client_id":"another-client","slug":"artidezi-fixture"}),
    ));
    for (path, status, body) in cases {
        let fixture = fixture();
        let forge = fixture.forge();
        fixture
            .state
            .lock()
            .unwrap()
            .overrides
            .insert(path.into(), Reply::new(status, body));
        assert!(forge.verify_installation().is_err(), "{path}");
        assert!(forge.tokens.lock().unwrap().is_empty());
    }
}

#[test]
fn all_repository_installations_still_require_single_repository_tokens() {
    for token_repositories in [vec!["org/app"], vec!["org/app", "org/other"]] {
        let fixture = fixture();
        let forge = fixture.forge();
        {
            let mut state = fixture.state.lock().unwrap();
            state.overrides.insert(
                "/app/installations/42".into(),
                Reply::new(200, json!({
                    "id":42, "app_id":17, "account":{"login":"org"},
                    "repository_selection":"all", "permissions":permissions(), "suspended_at":null
                })),
            );
            state.overrides.insert(
                "/app/installations/42/access_tokens".into(),
                Reply::new(201, json!({
                    "token":"fixture-installation-token-scoped",
                    "expires_at":(Utc::now()+Duration::hours(1)).to_rfc3339(),
                    "permissions":permissions(),
                    "repositories":token_repositories.iter().map(|name|json!({"full_name":name})).collect::<Vec<_>>()
                })),
            );
        }
        let result = forge.verify_installation();
        if token_repositories.len() == 1 {
            assert_eq!(result.unwrap(), vec!["org/app"]);
        } else {
            assert!(result.is_err(), "overbroad token scope must be rejected");
            assert!(forge.tokens.lock().unwrap().is_empty());
        }
        let before = {
            let state = fixture.state.lock().unwrap();
            let issuance = state.requests.iter().find(|r| r.method == "POST").unwrap();
            assert_eq!(issuance.path, "/app/installations/42/access_tokens");
            assert_eq!(issuance.body["repositories"], json!(["app"]));
            assert_eq!(issuance.body["permissions"], permissions());
            state.requests.len()
        };
        assert!(forge.git_credentials("org/other").is_err());
        assert!(forge.get_merge_request("org/other", 33).is_err());
        assert!(!forge.writes_allowed());
        assert_eq!(fixture.state.lock().unwrap().requests.len(), before);
    }
}

#[test]
fn backoff_stops_requests_and_redirects_never_receive_credentials() {
    for status in [429, 403, 302] {
        let fixture = fixture();
        let forge = fixture.forge();
        let retry = (Utc::now() + Duration::minutes(10)).timestamp();
        let mut reply = Reply::new(status, json!({"message":"sensitive-server-body"}));
        reply
            .headers
            .insert("location".into(), "http://127.0.0.1:1/leak".into());
        reply.headers.insert("retry-after".into(), "60".into());
        reply
            .headers
            .insert("x-ratelimit-remaining".into(), "0".into());
        reply
            .headers
            .insert("x-ratelimit-reset".into(), retry.to_string());
        fixture
            .state
            .lock()
            .unwrap()
            .overrides
            .insert("/repos/org/app".into(), reply);
        let error = forge.verify_installation().unwrap_err().to_string();
        assert!(!error.contains("sensitive-server-body"));
        if status != 302 {
            assert!(forge.retry_after().unwrap().timestamp() >= retry);
            let count = fixture.state.lock().unwrap().requests.len();
            assert!(forge.verify_installation().is_err());
            assert_eq!(fixture.state.lock().unwrap().requests.len(), count);
        }
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|r| r.path == "/repos/org/app")
                .count(),
            1
        );
    }
}

#[test]
fn configured_scope_branch_identity_and_pending_checks_are_enforced() {
    let fixture = fixture();
    let forge = fixture.forge();
    for remote in ["org/other", "other/app", "org/app?x=1", "../app"] {
        assert!(forge.get_merge_request(remote, 33).is_err());
        assert!(forge.git_credentials(remote).is_err());
    }
    assert!(fixture.state.lock().unwrap().requests.is_empty());
    fixture.state.lock().unwrap().overrides.insert(
        "/repos/org/app/branches/release%2Fv1".into(),
        Reply::new(
            200,
            json!({"name":"release/v1","commit":{"sha":"b".repeat(40)}}),
        ),
    );
    forge.branch_head("org/app", "release/v1").unwrap();
    for (conclusion, expected) in [
        ("none", ChecksState::Absent),
        ("running", ChecksState::Pending),
        ("failure", ChecksState::Failure),
        ("skipped", ChecksState::Inconclusive),
        ("success", ChecksState::Success),
    ] {
        fixture.state.lock().unwrap().conclusion = conclusion.into();
        assert_eq!(
            forge.get_merge_request("org/app", 33).unwrap().checks,
            expected
        );
    }
    fixture.state.lock().unwrap().open = false;
    assert_eq!(
        forge.get_merge_request("org/app", 33).unwrap().state,
        MergeRequestState::Closed
    );
    assert!(
        forge
            .list_open_merge_requests("org/app", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn publication_is_explicit_and_read_tokens_stay_narrow() {
    let fixture = fixture();
    fixture.state.lock().unwrap().publication = true;
    let read_only = fixture.forge();
    read_only.verify_installation().unwrap();
    assert!(!read_only.writes_allowed());
    assert!(read_only.git_push_credentials("org/app").is_err());
    let forge = fixture
        .forge()
        .with_publication_repositories(["org/app".into()].into())
        .unwrap();
    forge.verify_installation().unwrap();
    assert!(forge.writes_allowed_for("org/app"));
    assert!(!forge.writes_allowed_for("org/other"));
    assert!(forge.git_push_credentials("org/other").is_err());
    let fetch = forge.git_credentials("org/app").unwrap().unwrap();
    let push = forge.git_push_credentials("org/app").unwrap().unwrap();
    assert_ne!(fetch.token, push.token);
    let head = CommitSha("a".repeat(40));
    let review = forge
        .create_comment_review("org/app", 33, &head, "No adaptation needed.")
        .unwrap();
    assert_eq!(forge.list_reviews("org/app", 33).unwrap(), vec![review]);
    assert!(
        forge
            .merge("org/app", 33, &head, MergeMethod::Merge)
            .is_err()
    );
    assert!(
        forge
            .create_approval_review("org/app", 33, &head, "Approve")
            .is_err()
    );
    assert!(forge.create_tag("org/app", "v1", &head).is_err());
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.reviews.len(), 1);
    assert!(
        state
            .requests
            .iter()
            .filter(|r| r.path.ends_with("/reviews") && r.method == "POST")
            .all(|r| r.body["event"] == "COMMENT")
    );
}

#[test]
fn closing_comments_on_and_closes_only_an_open_owned_proposal() {
    let fixture = fixture();
    fixture.state.lock().unwrap().publication = true;
    let read_only = fixture.forge();
    let forge = fixture
        .forge()
        .with_publication_repositories(["org/app".into()].into())
        .unwrap();
    fixture
        .state
        .lock()
        .unwrap()
        .branches
        .insert("artidezi/task".into(), "c".repeat(40));
    let proposal = forge
        .create_merge_request(
            "org/app",
            &NewMergeRequest {
                title: "Adapt".into(),
                body: "Adaptation".into(),
                source_branch: "artidezi/task".into(),
                target_branch: "main".into(),
                draft: false,
            },
        )
        .unwrap();
    let head = CommitSha("c".repeat(40));
    let moved = CommitSha("d".repeat(40));
    let expected = crate::ExpectedProposal {
        number: proposal.number,
        source_branch: "artidezi/task",
        target_branch: "main",
        head: &head,
    };
    assert!(
        read_only
            .close_merge_request("org/app", &expected, "Superseded.")
            .is_err()
    );
    let source_head = CommitSha("a".repeat(40));
    let source = crate::ExpectedProposal {
        number: 33,
        source_branch: "renovate/library",
        target_branch: "main",
        head: &source_head,
    };
    assert!(
        forge
            .close_merge_request("org/app", &source, "Superseded.")
            .is_err(),
        "never closes a foreign source"
    );
    for (case, wrong) in [
        (
            "moved head",
            crate::ExpectedProposal {
                head: &moved,
                ..expected
            },
        ),
        (
            "other branch",
            crate::ExpectedProposal {
                source_branch: "artidezi/other",
                ..expected
            },
        ),
        (
            "other target",
            crate::ExpectedProposal {
                target_branch: "develop",
                ..expected
            },
        ),
    ] {
        assert!(
            forge
                .close_merge_request("org/app", &wrong, "Superseded.")
                .is_err(),
            "{case}"
        );
    }
    // A push after the caller's check is refused before any write.
    fixture
        .state
        .lock()
        .unwrap()
        .branches
        .insert("artidezi/task".into(), moved.0.clone());
    assert!(
        forge
            .close_merge_request("org/app", &expected, "Superseded.")
            .is_err()
    );
    assert!(fixture.state.lock().unwrap().comments.is_empty());
    assert_eq!(
        forge
            .get_merge_request("org/app", proposal.number)
            .unwrap()
            .state,
        MergeRequestState::Open
    );
    fixture
        .state
        .lock()
        .unwrap()
        .branches
        .insert("artidezi/task".into(), head.0.clone());
    forge
        .close_merge_request("org/app", &expected, "Superseded.")
        .unwrap();
    assert_eq!(
        forge
            .get_merge_request("org/app", proposal.number)
            .unwrap()
            .state,
        MergeRequestState::Closed
    );
    assert!(
        forge
            .close_merge_request("org/app", &expected, "Superseded.")
            .is_err()
    );
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == "GET" || !r.path.starts_with("/repos/org/app/pulls/33")),
        "the source is never written"
    );
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.comments.len(), 1);
    assert_eq!(state.comments[0]["number"], proposal.number);
    assert_eq!(state.comments[0]["body"], "Superseded.");
}

#[test]
fn workflow_push_uses_a_separate_single_repository_token() {
    let fixture = fixture();
    fixture.state.lock().unwrap().publication = true;
    let forge = fixture
        .forge()
        .with_publication_repositories(["org/app".into()].into())
        .unwrap();
    assert!(forge.git_push_workflow_credentials("org/app").is_err());
    fixture.state.lock().unwrap().workflow_permission = true;
    let ordinary = forge.git_push_credentials("org/app").unwrap().unwrap();
    let workflow = forge
        .git_push_workflow_credentials("org/app")
        .unwrap()
        .unwrap();
    assert_ne!(ordinary.token, workflow.token);
    assert!(forge.git_push_workflow_credentials("org/other").is_err());
    let state = fixture.state.lock().unwrap();
    let issued: Vec<_> = state
        .requests
        .iter()
        .filter(|request| request.path == "/app/installations/42/access_tokens")
        .map(|request| request.body.clone())
        .collect();
    assert_eq!(issued.len(), 2);
    assert_eq!(
        issued[0]["permissions"],
        crate::github::fixture::write_permissions()
    );
    assert_eq!(
        issued[1]["permissions"],
        crate::github::fixture::workflow_write_permissions()
    );
    assert_eq!(issued[1]["repositories"], serde_json::json!(["app"]));
}

#[test]
fn dry_run_rejects_overbroad_issued_token_even_when_installation_allows_writes() {
    let fixture = fixture();
    fixture.state.lock().unwrap().publication = true;
    fixture.state.lock().unwrap().overrides.insert("/app/installations/42/access_tokens".into(), Reply::new(201, json!({"token":"fixture-installation-token-overbroad", "expires_at":(Utc::now()+Duration::hours(1)).to_rfc3339(), "permissions":crate::github::fixture::write_permissions(),"repositories":[{"full_name":"org/app"}]})));
    assert!(fixture.forge().verify_installation().is_err());
}

#[test]
fn a_push_racing_the_close_reopens_the_proposal_at_the_new_head() {
    for fail_reopen in [false, true] {
        let fixture = fixture();
        fixture.state.lock().unwrap().publication = true;
        let forge = fixture
            .forge()
            .with_publication_repositories(["org/app".into()].into())
            .unwrap();
        let head = CommitSha("c".repeat(40));
        let pushed = "d".repeat(40);
        fixture
            .state
            .lock()
            .unwrap()
            .branches
            .insert("artidezi/task".into(), head.0.clone());
        let proposal = forge
            .create_merge_request(
                "org/app",
                &NewMergeRequest {
                    title: "Adapt".into(),
                    body: "Adaptation".into(),
                    source_branch: "artidezi/task".into(),
                    target_branch: "main".into(),
                    draft: false,
                },
            )
            .unwrap();
        {
            let mut state = fixture.state.lock().unwrap();
            state.push_before_close = Some(pushed.clone());
            state.fail_reopen = fail_reopen;
        }
        let error = forge
            .close_merge_request(
                "org/app",
                &crate::ExpectedProposal {
                    number: proposal.number,
                    source_branch: "artidezi/task",
                    target_branch: "main",
                    head: &head,
                },
                "Superseded.",
            )
            .unwrap_err()
            .to_string();
        let current = forge.get_merge_request("org/app", proposal.number).unwrap();
        assert_eq!(current.head_commit.0, pushed);
        if fail_reopen {
            assert!(error.contains("could not be reopened"), "{error}");
            assert_eq!(current.state, MergeRequestState::Closed);
        } else {
            assert!(error.contains("reopened it"), "{error}");
            assert_eq!(current.state, MergeRequestState::Open);
        }
        assert_eq!(fixture.state.lock().unwrap().comments.len(), 1);
    }
}
