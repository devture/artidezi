// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Disposable loopback GitHub-shaped API for adapter and controller tests. Never in normal builds.

use super::{GithubAppIdentity, GithubForge};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use openssl::{hash::MessageDigest, pkey::PKey, rsa::Rsa, sign::Verifier};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub headers: BTreeMap<String, String>,
    pub raw: Option<Vec<u8>>,
}
impl Reply {
    pub fn new(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            headers: BTreeMap::new(),
            raw: None,
        }
    }
}

pub struct State {
    pub base: String,
    pub head: String,
    pub title: String,
    pub open: bool,
    pub draft: bool,
    pub merged: bool,
    pub lose_merge_response: bool,
    pub merge_on_approval: bool,
    pub pending_on_approval: bool,
    pub fail_on_approval: bool,
    pub move_head_on_merge: bool,
    pub conclusion: String,
    pub requests: Vec<Request>,
    pub overrides: BTreeMap<String, Reply>,
    pub issued: usize,
    pub not_modified: usize,
    pub publication: bool,
    pub workflow_permission: bool,
    pub actions_permission: bool,
    pub branches: BTreeMap<String, String>,
    pub proposals: BTreeMap<u64, Value>,
    pub reviews: Vec<Value>,
    pub lose_create_response: bool,
    pub lose_review_response: bool,
}

pub struct Fixture {
    api: String,
    pem: String,
    pub state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    pub fn new(base: &str, head: &str) -> Self {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let pem = String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        let public = PKey::public_key_from_pem(&key.public_key_to_pem().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            base: base.into(),
            head: head.into(),
            title: "Update library".into(),
            open: true,
            draft: false,
            merged: false,
            lose_merge_response: false,
            merge_on_approval: false,
            pending_on_approval: false,
            fail_on_approval: false,
            move_head_on_merge: false,
            conclusion: "success".into(),
            requests: vec![],
            overrides: BTreeMap::new(),
            issued: 0,
            not_modified: 0,
            publication: false,
            workflow_permission: false,
            actions_permission: false,
            branches: BTreeMap::new(),
            proposals: BTreeMap::new(),
            reviews: vec![],
            lose_create_response: false,
            lose_review_response: false,
        }));
        let server = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut header = vec![];
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 64 * 1024);
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                let mut lines = header.lines();
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let path = first.next().unwrap().to_owned();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_lowercase(), v.trim().to_owned()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .map(|v| v.parse().unwrap())
                    .unwrap_or(0);
                assert!(length < 64 * 1024);
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).unwrap();
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                if path != "/signed-job-log" {
                    assert_eq!(
                        headers.get("x-github-api-version").map(String::as_str),
                        Some("2026-03-10")
                    );
                }
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.strip_prefix("Bearer "));
                if path == "/signed-job-log" {
                    assert!(
                        auth.is_none(),
                        "signed log host must not receive the App bearer"
                    );
                } else if path.starts_with("/app") {
                    let auth = auth.unwrap();
                    let parts: Vec<_> = auth.split('.').collect();
                    assert_eq!(parts.len(), 3);
                    let mut verifier = Verifier::new(MessageDigest::sha256(), &public).unwrap();
                    verifier
                        .update(format!("{}.{}", parts[0], parts[1]).as_bytes())
                        .unwrap();
                    assert!(
                        verifier
                            .verify(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap())
                            .unwrap(),
                        "JWT signature"
                    );
                    let claims: Value =
                        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
                    assert_eq!(claims["iss"], "Iv.fixture");
                    let now = Utc::now().timestamp();
                    assert!(
                        claims["iat"].as_i64().unwrap() <= now
                            && claims["iat"].as_i64().unwrap() >= now - 90
                    );
                    assert!(
                        claims["exp"].as_i64().unwrap() > now
                            && claims["exp"].as_i64().unwrap() <= now + 600
                    );
                } else {
                    assert!(auth.unwrap().starts_with("fixture-installation-token-"));
                }
                let request = Request {
                    method,
                    path,
                    headers,
                    body,
                };
                let mut state = server.lock().unwrap();
                let mut reply = state
                    .overrides
                    .get(&request.path)
                    .cloned()
                    .unwrap_or_else(|| reply(&request, &mut state));
                state.requests.push(request.clone());
                let mut body = reply
                    .raw
                    .take()
                    .unwrap_or_else(|| reply.body.to_string().into_bytes());
                if request.method == "GET" && reply.status == 200 {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    body.hash(&mut hasher);
                    let etag = format!("\"{:x}\"", hasher.finish());
                    if request.headers.get("if-none-match") == Some(&etag) {
                        reply.status = 304;
                        body.clear();
                        state.not_modified += 1;
                    }
                    reply.headers.insert("etag".into(), etag);
                }
                let extras = reply
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}\r\n"))
                    .collect::<String>();
                // Oversized-body tests deliberately close the client after reading headers.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} Fixture\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n{extras}\r\n",
                    reply.status,
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self {
            api,
            pem,
            state,
            stop,
            thread: Some(thread),
        }
    }
    pub fn forge(&self) -> GithubForge {
        GithubForge::at_for_fixture(
            self.identity(),
            &self.pem,
            ["org/app".into()].into(),
            &self.api,
        )
        .unwrap()
    }
    pub fn identity(&self) -> GithubAppIdentity {
        GithubAppIdentity {
            client_id: "Iv.fixture".into(),
            installation_id: 42,
            account: "org".into(),
            bot_login: "artidezi-fixture[bot]".into(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

pub fn permissions() -> Value {
    json!({"metadata":"read", "contents":"read", "pull_requests":"read", "checks":"read", "statuses":"read"})
}
fn reply(r: &Request, state: &mut State) -> Reply {
    let body = match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/app") => json!({"id":17,"client_id":"Iv.fixture","slug":"artidezi-fixture"}),
        ("GET", "/app/installations/42") => {
            json!({"id":42,"app_id":17,"account":{"login":"org"},"repository_selection":"selected","permissions":if state.workflow_permission { installation_with_workflow_permissions() } else if state.actions_permission { installation_with_actions_permissions() } else if state.publication { write_permissions() } else { permissions() },"suspended_at":null})
        }
        ("POST", "/app/installations/42/access_tokens") => {
            assert_eq!(r.body["repositories"], json!(["app"]));
            assert!(
                r.body["permissions"] == permissions()
                    || (state.publication && r.body["permissions"] == write_permissions())
                    || (state.workflow_permission
                        && r.body["permissions"] == workflow_write_permissions())
                    || (state.actions_permission
                        && r.body["permissions"] == actions_read_permissions())
            );
            state.issued += 1;
            return Reply::new(
                201,
                json!({"token":format!("fixture-installation-token-{}",state.issued),"expires_at":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339(),"permissions":r.body["permissions"],"repositories":[{"full_name":"org/app"}]}),
            );
        }
        ("GET", "/repos/org/app") => {
            json!({"full_name":"org/app","default_branch":"main","private":true,"archived":false,"disabled":false})
        }
        ("GET", "/repos/org/app/branches/main") => {
            json!({"name":"main","commit":{"sha":state.base}})
        }
        (
            "GET",
            "/repos/org/app/pulls?state=open&sort=created&direction=asc&per_page=100&page=1",
        ) => {
            if state.open {
                json!([{"number":33,"title":state.title,"user":{"login":"renovate[bot]","type":"Bot"},"state":"open","draft":state.draft,"head":{"ref":"renovate/library","sha":state.head,"repo":{"full_name":"a-fork/app"}},"base":{"ref":"main","sha":state.base,"repo":{"full_name":"org/app"}}}])
            } else {
                json!([])
            }
        }
        ("GET", "/repos/org/app/pulls/33") => {
            json!({"number":33,"title":state.title,"body":"Review compatibility","user":{"login":"renovate[bot]","type":"Bot"},"state":if state.open {"open"}else{"closed"},"merged":state.merged,"draft":state.draft,"head":{"ref":"renovate/library","sha":state.head,"repo":{"full_name":"a-fork/app"}},"base":{"ref":"main","sha":state.base,"repo":{"full_name":"org/app"}},"changed_files":1,"updated_at":"2026-09-13T00:00:00Z"})
        }
        ("POST", "/repos/org/app/pulls") => {
            assert!(state.publication);
            let n = state.proposals.len() as u64 + 100;
            let branch = r.body["head"].as_str().unwrap();
            let head = state.branches.get(branch).unwrap();
            let value = json!({"number":n,"title":r.body["title"],"body":r.body["body"],"user":{"login":"artidezi-fixture[bot]","type":"Bot"},"state":"open","merged":false,"draft":r.body["draft"],"head":{"ref":branch,"sha":head,"repo":{"full_name":"org/app"}},"base":{"ref":r.body["base"],"sha":state.base,"repo":{"full_name":"org/app"}},"changed_files":2,"updated_at":"2026-09-13T00:00:00Z"});
            state.proposals.insert(n, value.clone());
            if std::mem::take(&mut state.lose_create_response) {
                return Reply::new(503, json!({"message":"lost response"}));
            }
            return Reply::new(201, value);
        }
        ("PATCH", path) if path.starts_with("/repos/org/app/pulls/") => {
            let number = path.rsplit('/').next().unwrap().parse::<u64>().unwrap();
            let proposal = state.proposals.get_mut(&number).unwrap();
            proposal["title"] = r.body["title"].clone();
            proposal["body"] = r.body["body"].clone();
            proposal.clone()
        }
        ("GET", path) if path.starts_with("/repos/org/app/pulls?state=all&head=") => {
            json!(
                state
                    .proposals
                    .values()
                    .map(|v| json!({"number":v["number"]}))
                    .collect::<Vec<_>>()
            )
        }
        ("GET", "/repos/org/app/pulls/33/reviews?per_page=100&page=1") => json!(state.reviews),
        ("POST", "/repos/org/app/pulls/33/reviews") => {
            assert!(state.publication);
            assert!(r.body["event"] == "COMMENT" || r.body["event"] == "APPROVE");
            assert_eq!(r.body["commit_id"], state.head);
            let value = json!({"id":state.reviews.len()+1,"user":{"login":"artidezi-fixture[bot]","type":"Bot"},"commit_id":state.head,"body":r.body["body"],"state":if r.body["event"] == "APPROVE" { "APPROVED" } else { "COMMENTED" }});
            state.reviews.push(value.clone());
            if state.pending_on_approval && r.body["event"] == "APPROVE" {
                state.conclusion = "running".into();
            }
            if state.fail_on_approval && r.body["event"] == "APPROVE" {
                state.conclusion = "failure".into();
            }
            if state.merge_on_approval && r.body["event"] == "APPROVE" {
                state.open = false;
                state.merged = true;
            }
            if std::mem::take(&mut state.lose_review_response) {
                return Reply::new(503, json!({"message":"lost review response"}));
            }
            value
        }
        ("PUT", "/repos/org/app/pulls/33/merge") => {
            assert!(state.publication);
            if std::mem::take(&mut state.move_head_on_merge) {
                state.head = "d".repeat(40);
            }
            if r.body["sha"] != state.head {
                return Reply::new(409, json!({"message":"Head changed"}));
            }
            assert!(state.open && state.conclusion == "success");
            assert_eq!(r.body["merge_method"], "merge");
            state.open = false;
            state.merged = true;
            if std::mem::take(&mut state.lose_merge_response) {
                return Reply::new(503, json!({"message":"lost merge response"}));
            }
            json!({"merged":true,"sha":"e".repeat(40),"message":"Merged"})
        }
        ("GET", path)
            if path.starts_with("/repos/org/app/pulls/")
                && path
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .parse::<u64>()
                    .ok()
                    .is_some_and(|n| state.proposals.contains_key(&n)) =>
        {
            let mut proposal =
                state.proposals[&path.rsplit('/').next().unwrap().parse::<u64>().unwrap()].clone();
            let branch = proposal["head"]["ref"].as_str().unwrap().to_owned();
            if let Some(head) = state.branches.get(&branch) {
                proposal["head"]["sha"] = json!(head);
            }
            proposal
        }
        ("GET", path) if path.starts_with("/repos/org/app/branches/") => {
            let branch = path
                .trim_start_matches("/repos/org/app/branches/")
                .replace("%2F", "/");
            match state.branches.get(&branch) {
                Some(head) => json!({"name":branch,"commit":{"sha":head}}),
                None => return Reply::new(404, json!({"message":"not found"})),
            }
        }
        ("GET", path) if path.contains("/status?") => {
            json!({"sha":path.split('/').nth(5).unwrap(),"state":"pending","total_count":0,"statuses":[]})
        }
        ("GET", path) if path.contains("/check-runs?") => {
            if state.conclusion == "none" {
                json!({"total_count":0,"check_runs":[]})
            } else {
                json!({"total_count":1,"check_runs":[{"id":1,"name":"tests","head_sha":path.split('/').nth(5).unwrap(),"status":if state.conclusion=="running" {"in_progress"}else{"completed"},"conclusion":state.conclusion}]})
            }
        }
        ("GET", path) if path.starts_with("/repos/org/app/actions/runs?head_sha=") => {
            json!({"total_count":1,"workflow_runs":[{"id":9,"head_sha":state.head,"run_attempt":1,"repository":{"full_name":"org/app"}}]})
        }
        ("GET", "/repos/org/app/actions/runs/9/attempts/1/jobs?per_page=100&page=1") => {
            json!({"total_count":1,"jobs":[{"id":27,"run_id":9,"head_sha":state.head,"name":"tests","status":if state.conclusion == "running" {"in_progress"} else {"completed"},"conclusion":if state.conclusion == "running" {Value::Null} else {json!(state.conclusion)} }]})
        }
        ("GET", "/repos/org/app/actions/jobs/27") => {
            json!({"id":27,"run_id":9,"head_sha":state.head,"name":"tests","status":if state.conclusion == "running" {"in_progress"} else {"completed"},"conclusion":if state.conclusion == "running" {Value::Null} else {json!(state.conclusion)} })
        }
        ("GET", "/repos/org/app/actions/runs/9") => {
            json!({"id":9,"head_sha":state.head,"run_attempt":1,"repository":{"full_name":"org/app"}})
        }
        ("GET", "/repos/org/app/actions/jobs/27/logs") => {
            let mut reply = Reply::new(302, Value::Null);
            reply.headers.insert(
                "location".into(),
                format!("http://{}/signed-job-log", r.headers["host"]),
            );
            return reply;
        }
        ("GET", "/signed-job-log") => {
            let mut reply = Reply::new(200, Value::Null);
            reply.raw = Some(b"setup\n\x1b[31mfailed\x1b[0m\n".to_vec());
            return reply;
        }
        _ => panic!(
            "unexpected GitHub fixture operation: {} {}",
            r.method, r.path
        ),
    };
    Reply::new(200, body)
}

pub fn write_permissions() -> Value {
    json!({"metadata":"read", "contents":"write", "pull_requests":"write", "checks":"read", "statuses":"read"})
}

pub fn workflow_write_permissions() -> Value {
    json!({"metadata":"read", "contents":"write", "workflows":"write"})
}

fn installation_with_workflow_permissions() -> Value {
    json!({"metadata":"read", "contents":"write", "pull_requests":"write", "checks":"read", "statuses":"read", "workflows":"write"})
}

pub fn actions_read_permissions() -> Value {
    json!({"metadata":"read", "actions":"read"})
}

fn installation_with_actions_permissions() -> Value {
    json!({"metadata":"read", "contents":"read", "pull_requests":"read", "checks":"read", "statuses":"read", "actions":"read"})
}
