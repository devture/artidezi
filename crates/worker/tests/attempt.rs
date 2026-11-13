// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The supervisor driven end to end with the scripted fake harness.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use artidezi_contract::attempt::AttemptStatus;
use artidezi_contract::common::*;
use artidezi_contract::job::*;
use artidezi_git::bundle;
use artidezi_harness::codex::CodexHarness;
use artidezi_harness::fake::FakeHarness;
use artidezi_worker::{
    AttemptInputs, Cancellation, WorkerConfig, WorkerError, run_attempt, run_attempt_cancellable,
};
use git2::{Oid, Repository, Signature};

fn commit_file(repo: &Repository, path: &str, content: &str, message: &str) -> Oid {
    let workdir = repo.workdir().unwrap();
    let full = workdir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("Upstream", "up@invalid").unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .unwrap()
}

fn digest(bytes: &[u8]) -> ArtifactRef {
    use sha2::Digest;
    ArtifactRef {
        sha256: hex::encode(sha2::Sha256::digest(bytes)),
        size: bytes.len() as u64,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    base: Oid,
    snapshot: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let upstream_dir = root.join("upstream");
    let upstream = Repository::init_opts(
        &upstream_dir,
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
    commit_file(&upstream, "README.md", "hello\n", "Initial");
    let base = commit_file(
        &upstream,
        "defaults/main.yml",
        "version: 1.0.0\n",
        "Add defaults",
    );
    let snapshot = root.join("snapshot.bundle");
    bundle::create(&upstream_dir, &snapshot, &["refs/heads/main"]).unwrap();
    Fixture {
        _dir: dir,
        root,
        base,
        snapshot,
    }
}

fn make_job(f: &Fixture, attempt_id: &str, wall_clock: u64) -> (Job, Vec<u8>) {
    let snapshot_bytes = fs::read(&f.snapshot).unwrap();
    let job = Job {
        schema_version: SCHEMA_VERSION,
        attempt_id: attempt_id.into(),
        task_id: "tsk_1".into(),
        attempt_number: 1,
        prompt_template_version: 1,
        agent_profile: AgentProfile {
            name: "fake".into(),
            harness: Harness::Fake,
            harness_version_constraint: "*".into(),
            model: "none".into(),
            model_settings: BTreeMap::new(),
            model_endpoint: ModelEndpointRef {
                auth_mode: "none".into(),
                credential_ref: "none".into(),
                base_url: None,
            },
        },
        worker_requirements: WorkerRequirements {
            harness_sandbox: HarnessSandbox::Disabled,
            required_tags: vec![],
        },
        network: NetworkPolicy {
            command_egress: CommandEgress {
                mode: EgressMode::Blocked,
                allowed_domains: None,
                denied_domains: vec![],
                private_network: PrivateNetworkMode::Blocked,
                allowed_private_endpoints: vec![],
            },
            web_search: WebSearch {
                mode: WebSearchMode::Disabled,
                domain_scope: WebSearchScope::PublicInternet,
                allowed_domains: None,
            },
        },
        limits: Limits {
            wall_clock_seconds: wall_clock,
            result_json_max_bytes: 65536,
            max_commits_per_repository: 5,
            max_diff_bytes_per_repository: 65536,
            max_commit_message_bytes: 1024,
        },
        allowed_outcomes: vec![Outcome::Proposal, Outcome::NeedsHuman, Outcome::Failed],
        repositories: vec![
            JobRepository {
                id: "role".into(),
                access: RepositoryAccess::ReadWrite,
                branch: "main".into(),
                base_commit: CommitSha(f.base.to_string()),
                snapshot_bundle: digest(&snapshot_bytes),
                input_refs: vec![],
                setup_recipe: vec![RecipeStep {
                    name: "noop".into(),
                    command: vec!["true".into()],
                    timeout_seconds: 10,
                    cwd: ".".into(),
                }],
                verification_recipe: vec![RecipeStep {
                    name: "check-defaults".into(),
                    command: vec![
                        "grep".into(),
                        "-q".into(),
                        "1.1.0".into(),
                        "defaults/main.yml".into(),
                    ],
                    timeout_seconds: 10,
                    cwd: ".".into(),
                }],
                allowed_paths: None,
                tagging: Some(TaggingPolicy {
                    agent_may_propose: true,
                    allowed_pattern: r"v[0-9]+\.[0-9]+\.[0-9]+-[0-9]+".into(),
                }),
            },
            JobRepository {
                id: "context".into(),
                access: RepositoryAccess::ReadOnly,
                branch: "main".into(),
                base_commit: CommitSha(f.base.to_string()),
                snapshot_bundle: digest(&snapshot_bytes),
                input_refs: vec![],
                setup_recipe: vec![],
                verification_recipe: vec![],
                allowed_paths: None,
                tagging: None,
            },
        ],
        task_document: digest(b"task"),
        previous_attempt: None,
    };
    let bytes = serde_json::to_vec_pretty(&job).unwrap();
    (job, bytes)
}

fn script(f: &Fixture, name: &str, body: &str) -> std::path::PathBuf {
    let path = f.root.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -e\nif [ \"${{1:-}}\" = --version ]; then echo codex-cli 0.163.0; exit 0; fi\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(f: &Fixture) -> WorkerConfig {
    WorkerConfig {
        worker_pool: "personal".into(),
        worker_id: "w1".into(),
        workspaces_root: f.root.join("workspaces"),
        credentials_root: None,
        max_log_bytes: 4096,
        system_tmp: None,
    }
}

fn inputs<'a>(f: &Fixture, job: &'a Job, bytes: &'a [u8]) -> AttemptInputs<'a> {
    AttemptInputs {
        job,
        job_bytes: bytes,
        task_document: b"# Task\nDo the thing.\n",
        snapshot_bundles: BTreeMap::from([
            ("role".into(), f.snapshot.clone()),
            ("context".into(), f.snapshot.clone()),
        ]),
        input_ref_bundles: BTreeMap::new(),
        previous: None,
    }
}

/// Writes `$ARTIDEZI_RESULT_PATH` from a here-string so the shell script stays a plain string here.
fn result_writer(json: &str) -> String {
    format!(
        "printf '%s' '{}' > \"$ARTIDEZI_RESULT_PATH\"",
        json.replace('\'', "'\\''")
    )
}

fn codex_auth(suffix: &str) -> String {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": format!("access-{suffix}"),
            "account_id": "account-1",
            "id_token": format!("id-{suffix}"),
            "refresh_token": format!("refresh-{suffix}")
        }
    })
    .to_string()
}

fn codex_job_and_config(f: &Fixture, attempt_id: &str) -> (Job, WorkerConfig) {
    let (mut job, _) = make_job(f, attempt_id, 60);
    job.agent_profile.harness = Harness::Codex;
    job.agent_profile.model_endpoint.auth_mode = "chatgpt_login".into();
    job.agent_profile.model_endpoint.credential_ref = "codex-login".into();
    let credentials_root = f.root.join("credentials");
    let credential_dir = credentials_root.join("codex-login");
    fs::create_dir_all(&credential_dir).unwrap();
    fs::write(credential_dir.join("auth.json"), codex_auth("old")).unwrap();
    let mut config = config(f);
    config.credentials_root = Some(credentials_root);
    (job, config)
}

fn proposal_script() -> String {
    let json = r#"{"schema_version": 1, "outcome": "proposal", "summary": "Bumped to 1.1.0",
 "risk_hint": {"level": "low", "rationale": "patch"}, "evidence": [],
 "repositories": [{"id": "role", "changed": true, "head_commit": "HEADSHA", "tag_proposal": "v1.1.0-0"}]}"#;
    format!(
        "test \"$ARTIDEZI_FAKE_MODEL\" = none\n\
         sed -i 's/1.0.0/1.1.0/' defaults/main.yml\n\
         git add defaults/main.yml\n\
         git commit -q -m 'Update to 1.1.0'\n\
         HEAD=$(git rev-parse HEAD)\n\
         {}\n\
         sed -i \"s/HEADSHA/$HEAD/\" \"$ARTIDEZI_RESULT_PATH\"\n\
         echo session_id=fake-session\n\
         echo input_tokens=10\n",
        result_writer(json)
    )
}

#[test]
fn cancellation_stops_each_execution_stage_and_retains_partial_work() {
    for stage in ["setup", "harness", "verification"] {
        let f = fixture();
        let (mut job, _) = make_job(&f, "att_cancel", 60);
        let config = config(&f);
        let workspace = config.workspaces_root.join("att_cancel");
        let waiting = script(
            &f,
            "wait.sh",
            r#"
echo ready > "$ARTIDEZI_WORKSPACE/out/ready"
echo partial-log
sleep 60
echo unexpected > "$ARTIDEZI_WORKSPACE/out/finished"
"#,
        );
        let step = RecipeStep {
            name: "wait".into(),
            command: vec![waiting.to_str().unwrap().into()],
            timeout_seconds: 60,
            cwd: ".".into(),
        };
        let later = RecipeStep {
            name: "later".into(),
            command: vec![
                "sh".into(),
                "-c".into(),
                "touch \"$ARTIDEZI_WORKSPACE/out/later\"".into(),
            ],
            timeout_seconds: 10,
            cwd: ".".into(),
        };
        let harness = FakeHarness {
            script: if stage == "harness" {
                waiting
            } else {
                script(
                    &f,
                    "agent.sh",
                    &format!(
                        "echo ran > \"$ARTIDEZI_WORKSPACE/out/harness-ran\"\n{}",
                        proposal_script()
                    ),
                )
            },
        };
        match stage {
            "setup" => job.repositories[0].setup_recipe = vec![step, later],
            "verification" => job.repositories[0].verification_recipe = vec![step, later],
            _ => {}
        }
        let bytes = serde_json::to_vec(&job).unwrap();
        let cancellation = Cancellation::default();
        let signal = cancellation.clone();
        let ready = workspace.join("out/ready");
        let canceller = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            signal.cancel();
            assert!(ready.exists(), "execution did not reach the selected stage");
        });
        let result =
            run_attempt_cancellable(&config, &harness, &inputs(&f, &job, &bytes), &cancellation);
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(WorkerError::Cancelled)),
            "stage: {stage}"
        );
        assert!(workspace.join("out/ready").exists());
        assert!(!workspace.join("out/finished").exists());
        assert!(!workspace.join("out/later").exists());
        assert!(!workspace.join("out/attempt.json").exists());
        if stage == "setup" {
            assert!(!workspace.join("out/harness-ran").exists());
        }
    }
}

#[test]
fn codex_rotated_login_is_persisted_after_success() {
    let f = fixture();
    let (job, config) = codex_job_and_config(&f, "att_codex_auth");
    let result = r#"{"schema_version":1,"outcome":"needs_human","summary":"done","questions_for_human":["review"],"repositories":[{"id":"role","changed":false}]}"#;
    let body = format!(
        "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\n{}",
        codex_auth("new").replace('\'', "'\\''"),
        result_writer(result)
    );
    let harness = CodexHarness {
        binary: script(&f, "codex.sh", &body),
    };
    let bytes = serde_json::to_vec(&job).unwrap();

    let output = run_attempt(&config, &harness, &inputs(&f, &job, &bytes)).unwrap();
    assert_eq!(output.record.status, AttemptStatus::Completed);
    assert_eq!(
        fs::read_to_string(
            config
                .credentials_root
                .unwrap()
                .join("codex-login/auth.json")
        )
        .unwrap(),
        codex_auth("new")
    );
}

#[test]
fn codex_retries_prework_401_after_refresh_without_losing_the_first_log() {
    let f = fixture();
    let (job, config) = codex_job_and_config(&f, "att_codex_auth_retry");
    let result = r#"{"schema_version":1,"outcome":"needs_human","summary":"done","questions_for_human":["review"],"repositories":[{"id":"role","changed":false}]}"#;
    let body = format!(
        "if [ ! -e \"$CODEX_HOME/retried\" ]; then
           touch \"$CODEX_HOME/retried\"
           printf '%s' '{}' > \"$CODEX_HOME/auth.json\"
           printf '%s\\n' '{{\"type\":\"thread.started\"}}' '{{\"type\":\"turn.started\"}}' '{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"401 Unauthorized: Incorrect API key provided\"}}}}'
           exit 1
         fi
         {}",
        codex_auth("new").replace('\'', "'\\''"),
        result_writer(result)
    );
    let harness = CodexHarness {
        binary: script(&f, "codex-retry.sh", &body),
    };
    let bytes = serde_json::to_vec(&job).unwrap();

    let output = run_attempt(&config, &harness, &inputs(&f, &job, &bytes)).unwrap();
    assert_eq!(output.record.status, AttemptStatus::Completed);
    assert_eq!(output.record.harness.exit_code, Some(0));
    assert!(output.record.result.is_some());
    assert!(
        output
            .record
            .artifacts
            .iter()
            .any(|artifact| artifact.name == "harness-auth-retry-1-stdout.log")
    );
    assert_eq!(
        fs::read_to_string(
            config
                .credentials_root
                .unwrap()
                .join("codex-login/auth.json")
        )
        .unwrap(),
        codex_auth("new")
    );
}

#[test]
fn codex_rotated_login_is_persisted_after_cancellation() {
    let f = fixture();
    let (job, config) = codex_job_and_config(&f, "att_codex_cancel");
    let workspace = config.workspaces_root.join("att_codex_cancel");
    let body = format!(
        "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\n\
         touch \"$ARTIDEZI_WORKSPACE/out/ready\"\n\
         sleep 60",
        codex_auth("new").replace('\'', "'\\''")
    );
    let harness = CodexHarness {
        binary: script(&f, "codex-slow.sh", &body),
    };
    let bytes = serde_json::to_vec(&job).unwrap();
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let ready = workspace.join("out/ready");
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        signal.cancel();
        assert!(
            ready.exists(),
            "Codex fixture did not rotate its credential"
        );
    });

    let result =
        run_attempt_cancellable(&config, &harness, &inputs(&f, &job, &bytes), &cancellation);
    canceller.join().unwrap();
    assert!(matches!(result, Err(WorkerError::Cancelled)));
    assert_eq!(
        fs::read_to_string(
            config
                .credentials_root
                .unwrap()
                .join("codex-login/auth.json")
        )
        .unwrap(),
        codex_auth("new")
    );
}

#[test]
fn proposal_is_collected_and_verified() {
    let f = fixture();
    let (job, bytes) = make_job(&f, "att_ok", 60);
    let harness = FakeHarness {
        script: script(&f, "agent.sh", &proposal_script()),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    let record = &output.record;
    assert_eq!(record.status, AttemptStatus::Completed, "{record:#?}");
    assert_eq!(record.harness.exit_code, Some(0));
    assert_eq!(record.harness.session_id.as_deref(), Some("fake-session"));
    assert_eq!(record.harness.usage.as_ref().unwrap().input_tokens, 10);
    let result = record.result.as_ref().unwrap();
    assert!(
        result.validation_errors.is_empty(),
        "{:?}",
        result.validation_errors
    );

    let role = &record.repositories[0];
    assert_eq!(role.id.as_str(), "role");
    let series = role.series.as_ref().unwrap();
    assert!(series.valid, "{:?}", series.problems);
    assert_eq!(series.commits.len(), 1);
    assert_eq!(series.commits[0].message.trim(), "Update to 1.1.0");
    assert_eq!(role.head_commit, series.commits[0].sha);
    assert!(series.bundle.is_some() && series.diff.is_some());
    assert_eq!(role.setup.len(), 1);
    assert_eq!(role.setup[0].exit_code, Some(0));
    assert_eq!(role.verification.len(), 1);
    assert_eq!(role.verification[0].exit_code, Some(0));

    let context = &record.repositories[1];
    assert!(!context.read_only_violation);
    assert!(context.series.is_none());

    assert!(
        output
            .artifacts
            .iter()
            .any(|a| a.name == "role-commits.bundle")
    );
    assert!(output.artifacts.iter().all(|a| a.path.is_file()));
    assert_eq!(record.artifacts.len(), output.artifacts.len());
    assert!(output.workspace.join("out/attempt.json").is_file());
    output.cleanup().unwrap();
    assert!(!output.workspace.exists());
}

#[test]
fn inconsistent_head_and_dirty_tree_are_recorded() {
    let f = fixture();
    let (job, bytes) = make_job(&f, "att_bad", 60);
    let json = r#"{"schema_version": 1, "outcome": "proposal", "summary": "x",
 "repositories": [{"id": "role", "changed": true, "head_commit": "0000000000000000000000000000000000000000"}]}"#;
    let body = format!(
        "sed -i 's/1.0.0/1.1.0/' defaults/main.yml\n\
         git add defaults/main.yml\n\
         git commit -q -m Update\n\
         echo dirty >> README.md\n\
         {}\n",
        result_writer(json)
    );
    let harness = FakeHarness {
        script: script(&f, "agent.sh", &body),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    let series = output.record.repositories[0].series.as_ref().unwrap();
    assert!(!series.valid);
    assert!(
        series
            .problems
            .iter()
            .any(|p| p.contains("modified tracked")),
        "{:?}",
        series.problems
    );
    assert!(
        series
            .problems
            .iter()
            .any(|p| p.contains("agent reported head")),
        "{:?}",
        series.problems
    );
    assert_eq!(
        output.record.repositories[0].dirty_tracked_files,
        vec!["README.md".to_string()]
    );
    // No verification for an invalid series.
    assert!(output.record.repositories[0].verification.is_empty());
}

#[test]
fn missing_result_and_timeout_are_facts_not_errors() {
    let f = fixture();
    let (job, bytes) = make_job(&f, "att_none", 60);
    let harness = FakeHarness {
        script: script(&f, "agent.sh", "exit 3"),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    assert_eq!(output.record.status, AttemptStatus::Completed);
    assert_eq!(output.record.harness.exit_code, Some(3));
    assert!(output.record.result.is_none());

    let (job, bytes) = make_job(&f, "att_slow", 1);
    let harness = FakeHarness {
        script: script(&f, "slow.sh", "sleep 30"),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    assert_eq!(output.record.status, AttemptStatus::Timeout);
}

#[test]
fn read_only_modification_is_a_violation() {
    let f = fixture();
    let (job, bytes) = make_job(&f, "att_ro", 60);
    let json = r#"{"schema_version": 1, "outcome": "needs_human", "summary": "x", "questions_for_human": ["why?"],
 "repositories": [{"id": "role", "changed": false}]}"#;
    let body = format!(
        "cd \"$ARTIDEZI_WORKSPACE/repos/context\"\n\
         echo tamper >> README.md\n\
         git commit -q -am tamper\n\
         {}\n",
        result_writer(json)
    );
    let harness = FakeHarness {
        script: script(&f, "agent.sh", &body),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    assert!(output.record.repositories[1].read_only_violation);
    assert!(
        output
            .record
            .result
            .as_ref()
            .unwrap()
            .validation_errors
            .is_empty()
    );
}

#[test]
fn every_stage_shares_a_private_attempt_scratch_directory() {
    let f = fixture();
    let (mut job, _) = make_job(&f, "att_tmp", 60);
    let sh = |script: &str| vec!["sh".into(), "-c".into(), script.into()];
    let repo = &mut job.repositories[0];
    repo.setup_recipe[0].command =
        sh("test \"$TMPDIR\" = \"$ARTIDEZI_WORKSPACE/tmp\" && touch \"$TMPDIR/setup\"");
    repo.verification_recipe[0].command = sh("test -f \"$TMPDIR/agent\"");
    let bytes = serde_json::to_vec_pretty(&job).unwrap();
    let body = format!(
        "test -f \"$TMPDIR/setup\"\n\
         case \"$(mktemp)\" in \"$TMPDIR\"/*) ;; *) exit 9 ;; esac\n\
         touch \"$TMPDIR/agent\"\n{}",
        proposal_script()
    );
    let harness = FakeHarness {
        script: script(&f, "agent.sh", &body),
    };
    let output = run_attempt(&config(&f), &harness, &inputs(&f, &job, &bytes)).unwrap();
    let role = &output.record.repositories[0];
    assert_eq!(role.setup[0].exit_code, Some(0), "{role:#?}");
    assert_eq!(output.record.harness.exit_code, Some(0));
    assert_eq!(role.verification[0].exit_code, Some(0), "{role:#?}");
    let tmp = output.workspace.join("tmp");
    assert_eq!(
        fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(tmp.join("agent").is_file());
    output.cleanup().unwrap();
    assert!(!output.workspace.exists());
}
