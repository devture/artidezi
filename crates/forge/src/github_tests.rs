// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

struct Mock {
    reader: GithubReader,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn mock_server(handler: impl Fn(&str, usize) -> (u16, Value, String) + Send + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = requests.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(e) => panic!("mock accept failed: {e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                bytes.extend_from_slice(&byte);
            }
            let request = String::from_utf8(bytes).unwrap();
            let path = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let count = captured
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.lines().next() == request.lines().next())
                .count();
            let (status, body, extra) = handler(path, count);
            captured.lock().unwrap().push(request);
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len()).unwrap();
        }
    });
    Mock {
        reader: GithubReader::at(&api, Some("test-only-secret".into())),
        requests,
        stop,
        thread: Some(thread),
    }
}
fn pull() -> Value {
    json!({"number":33,"title":"Update dependency","body":"Untrusted release notes",
        "user":{"login":"renovate[bot]","type":"Bot"},"state":"open","merged":false,"draft":false,
        "head":{"ref":"renovate/dependency","sha":"a".repeat(40),"repo":{"full_name":"contributor/app"}},
        "base":{"ref":"main","sha":"b".repeat(40),"repo":{"full_name":"org/app"}},
        "changed_files":1,"updated_at":"2026-09-13T00:00:00Z"})
}
fn run(id: u64, conclusion: &str) -> Value {
    json!({"id":id,"name":"tests","status":"completed","conclusion":conclusion,"head_sha":"a".repeat(40)})
}
fn reply(path: &str) -> Value {
    if path == "/repos/org/app" {
        json!({"full_name":"org/app","default_branch":"main","private":false,"archived":false,"disabled":false})
    } else if path == "/repos/org/app/pulls/33" {
        pull()
    } else if path.contains("/status?") {
        json!({"sha":"a".repeat(40),"state":"pending","total_count":0,"statuses":[]})
    } else if path.contains("/check-runs?") {
        json!({"total_count":1,"check_runs":[run(1,"success")]})
    } else if path.contains("/files?") {
        json!([{"filename":"version.yml","status":"modified","additions":1,"deletions":1,"patch":"@@ -1 +1 @@\n-old\n+new"}])
    } else {
        panic!("unexpected local endpoint: {path}")
    }
}

#[test]
fn reads_only_get_with_explicit_auth_and_returns_pinned_source() {
    let mock = mock_server(|path, _| (200, reply(path), String::new()));
    let report = mock.reader.inspect("org/app", 33).unwrap();
    assert_eq!(report.checks_summary, "success"); // GitHub Actions without legacy commit statuses.
    assert_eq!(report.source.user.login, "renovate[bot]");
    assert_eq!(report.source_ref, "refs/pull/33/head");
    assert_eq!(report.source_url, "https://github.com/org/app/pull/33");
    assert_eq!(report.changed_files.len(), 1);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|r| {
        r.starts_with("GET /repos/org/app")
            && r.to_lowercase()
                .contains("authorization: bearer test-only-secret")
            && r.to_lowercase()
                .contains("x-github-api-version: 2026-03-10")
    }));
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("test-only-secret")
    );
}

#[test]
fn fails_when_source_moves_or_targets_another_repository() {
    for moved in [true, false] {
        let mock = mock_server(move |path, count| {
            let mut data = reply(path);
            if path == "/repos/org/app/pulls/33" {
                if moved && count == 1 {
                    data["head"]["sha"] = json!("c".repeat(40));
                }
                if !moved {
                    data["base"]["repo"]["full_name"] = json!("another/app");
                }
            }
            (200, data, String::new())
        });
        assert!(mock.reader.inspect("org/app", 33).is_err());
    }
}

#[test]
fn checks_failure_pending_and_absence_do_not_become_success() {
    let statuses = vec![CommitStatus {
        context: "lint".into(),
        state: "success".into(),
    }];
    let check = |conclusion| serde_json::from_value::<CheckRun>(run(1, conclusion)).unwrap();
    assert_eq!(summarize_checks("pending", &[], &[]), "absent");
    assert_eq!(
        summarize_checks("success", &statuses, &[check("failure")]),
        "failure"
    );
    assert_eq!(
        summarize_checks("pending", &statuses, &[check("success")]),
        "pending"
    );
    assert_eq!(
        summarize_checks("failure", &statuses, &[check("success")]),
        "failure"
    );
    assert_eq!(
        summarize_checks("pending", &[], &[check("skipped"), check("neutral")]),
        "inconclusive"
    );
    assert_eq!(
        summarize_checks("success", &statuses, &[check("unknown")]),
        "pending"
    );
    let mut running = check("success");
    running.status = "in_progress".into();
    assert_eq!(
        summarize_checks("success", &statuses, &[running]),
        "pending"
    );
    let failed = check("failure");
    let mut running = check("success");
    running.status = "in_progress".into();
    assert_eq!(
        summarize_checks("failure", &statuses, &[failed, running]),
        "pending"
    );
}

#[test]
fn reads_every_check_page_and_rejects_incomplete_lists() {
    let mock = mock_server(|path, _| {
        let data = if path.contains("/check-runs?") {
            let runs = if path.ends_with("page=1") {
                (1..=100).map(|id| run(id, "success")).collect::<Vec<_>>()
            } else {
                vec![run(101, "failure")]
            };
            json!({"total_count":101,"check_runs":runs})
        } else {
            reply(path)
        };
        (200, data, String::new())
    });
    let report = mock.reader.inspect("org/app", 33).unwrap();
    assert_eq!(report.check_runs.len(), 101);
    assert_eq!(report.checks_summary, "failure");
    drop(mock);
    for endpoint in ["/check-runs?", "/status?", "/files?"] {
        let mock = mock_server(move |path, _| {
            let mut data = reply(path);
            if path.contains(endpoint) {
                data = if endpoint == "/files?" {
                    json!([])
                } else if endpoint == "/status?" {
                    json!({"sha":"a".repeat(40),"state":"success","total_count":1,"statuses":[]})
                } else {
                    json!({"total_count":1,"check_runs":[]})
                };
            }
            (200, data, String::new())
        });
        assert!(mock.reader.inspect("org/app", 33).is_err());
    }
}

#[test]
fn rejects_injected_paths_without_a_request() {
    let mock = mock_server(|path, _| (200, reply(path), String::new()));
    for remote in [
        "../app",
        "org/..",
        "org/app/pulls",
        "https://github.com/org/app",
        "org/app?token=x",
        "org/app#fragment",
    ] {
        assert!(mock.reader.inspect(remote, 33).is_err());
    }
    assert!(mock.reader.inspect("org/app", 0).is_err());
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[test]
fn refuses_redirects_and_redacts_error_bodies() {
    for status in [301, 401, 403, 404, 429, 500] {
        let mock = mock_server(move |_, _| {
            (
                status,
                json!({"message":"test-only-secret"}),
                "Location: http://127.0.0.1:1/leak\r\n".into(),
            )
        });
        let error = mock.reader.inspect("org/app", 33).unwrap_err().to_string();
        assert!(!error.contains("test-only-secret"));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}
