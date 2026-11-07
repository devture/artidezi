// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use artidezi_worker::{
    Cancellation, WorkerError,
    client::Client,
    process::{self, Spec},
};

#[test]
fn only_an_authoritative_heartbeat_conflict_revokes_work() {
    for status in [204, 409, 401, 503] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let body = if status == 204 {
                ""
            } else {
                r#"{"error":"response from controller"}"#
            };
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let result = Client::new(&url, "test-token").heartbeat("att_test", "worker");
        server.join().unwrap();
        match status {
            204 => assert!(result.is_ok()),
            409 => assert!(matches!(result, Err(WorkerError::Cancelled))),
            _ => assert!(matches!(result, Err(WorkerError::Http { .. }))),
        }
    }
}

#[test]
fn cancellation_interrupts_blocked_stdin_and_terminates_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("child.pid");
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let observed = ready.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !observed.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        signal.cancel();
        assert!(observed.exists());
    });
    let args = vec![
        "-c".into(),
        "sh -c 'trap \"\" TERM; echo $$ > child.pid; exec sleep 60' & wait".into(),
    ];
    let stdin = vec![b'x'; 2 * 1024 * 1024];
    let started = Instant::now();
    let result = process::run_cancellable(
        &Spec {
            program: Path::new("/bin/sh"),
            args: &args,
            env: &[],
            cwd: dir.path(),
            stdin: Some(&stdin),
            timeout: Duration::from_secs(30),
            stdout_path: &dir.path().join("stdout"),
            stderr_path: &dir.path().join("stderr"),
        },
        &cancellation,
    );
    canceller.join().unwrap();
    assert!(matches!(result, Err(WorkerError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(15));
    let pid = std::fs::read_to_string(ready).unwrap();
    let status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.trim().is_empty() || status.trim().starts_with('Z'),
        "descendant still running: {status}"
    );
}

#[test]
fn timeout_escalates_to_sigkill_and_returns_a_timeout_record() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let outcome = process::run(&Spec {
        program: Path::new("/bin/sh"),
        args: &["-c".into(), "trap '' TERM; exec sleep 60".into()],
        env: &[],
        cwd: dir.path(),
        stdin: None,
        timeout: Duration::from_millis(200),
        stdout_path: &dir.path().join("stdout"),
        stderr_path: &dir.path().join("stderr"),
    })
    .unwrap();
    assert!(outcome.timed_out);
    assert_eq!(outcome.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(16));
}

#[test]
fn sigterm_cancels() {
    let cancellation = Cancellation::default();
    cancellation.on_terminate().unwrap();
    assert!(!cancellation.is_cancelled());
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
    assert!(cancellation.is_cancelled());
}
