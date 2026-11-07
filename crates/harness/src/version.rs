// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Seek};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::{HarnessError, Result};

pub(crate) fn installed_version(binary: &Path) -> Result<String> {
    probe(binary, Duration::from_secs(10))
}

fn probe(binary: &Path, timeout: Duration) -> Result<String> {
    let io = |source| HarnessError::Io {
        path: binary.into(),
        source,
    };
    // Files avoid both pipe-buffer deadlocks and waiting on stdout inherited by descendants.
    let mut stdout = tempfile::tempfile().map_err(io)?;
    let stderr = tempfile::tempfile().map_err(io)?;
    let mut child = Command::new(binary)
        .arg("--version")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(io)?)
        .stderr(stderr)
        .spawn()
        .map_err(io)?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            if !status.success() {
                return Err(HarnessError::Invalid(format!(
                    "{} --version failed",
                    binary.display()
                )));
            }
            stdout.rewind().map_err(io)?;
            let mut bytes = vec![];
            stdout.take(4097).read_to_end(&mut bytes).map_err(io)?;
            if bytes.len() > 4096 {
                return Err(HarnessError::Invalid(
                    "harness version output exceeds 4096 bytes".into(),
                ));
            }
            return String::from_utf8_lossy(&bytes)
                .split_whitespace()
                .last()
                .map(str::to_owned)
                .ok_or_else(|| HarnessError::Invalid("empty harness version output".into()));
        }
        if started.elapsed() >= timeout {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
    let killed = Instant::now();
    while killed.elapsed() < Duration::from_secs(5) {
        if child.try_wait().map_err(io)?.is_some() {
            return Err(HarnessError::Invalid(format!(
                "{} --version timed out",
                binary.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(HarnessError::UnstoppableProcess {
        program: binary.into(),
        pid: child.id(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn probes_reject_failure_empty_output_and_hangs_without_waiting_for_inherited_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("harness");
        for (body, expected) in [
            ("echo codex-cli 1.2.3", Some("1.2.3")),
            ("exit 3", None),
            ("exit 0", None),
            ("sleep 30", None),
            ("sleep 1 &\necho codex-cli 1.2.4", Some("1.2.4")),
        ] {
            std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let started = Instant::now();
            let result = probe(&script, Duration::from_millis(100));
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(result.ok().as_deref(), expected);
        }
    }
}
