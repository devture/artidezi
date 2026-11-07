// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Running a child process with a wall-clock limit, its own process group and log capture to files.

use std::fs::File;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::{Result, WorkerError, io_err};

/// Shared, one-way cancellation for one attempt's managed processes and later stages.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Cancels on SIGTERM, which PID 1 in a container otherwise ignores.
    pub fn on_terminate(&self) -> std::io::Result<()> {
        signal_hook::flag::register(signal_hook::consts::SIGTERM, self.0.clone()).map(|_| ())
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(WorkerError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub struct Spec<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
    pub stdout_path: &'a Path,
    pub stderr_path: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

const TERM_GRACE: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

// Keep the exit observation injectable: an uninterruptible kernel wait cannot safely be
// reproduced by an integration test. Neither grace period may call blocking Child::wait.
fn terminate(
    mut poll: impl FnMut() -> std::io::Result<Option<ExitStatus>>,
    mut signal: impl FnMut(Signal),
    term_grace: Duration,
    kill_grace: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    signal(Signal::SIGTERM);
    let wait = |poll: &mut dyn FnMut() -> std::io::Result<Option<ExitStatus>>, grace| {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = poll()? {
                return Ok(Some(status));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL.min(remaining));
        }
    };
    let exited = wait(&mut poll, term_grace);
    // Kill descendants even when their parent exited on SIGTERM or exit observation failed.
    signal(Signal::SIGKILL);
    match exited? {
        Some(status) => Ok(Some(status)),
        None => wait(&mut poll, kill_grace),
    }
}

/// Runs the process with a clean environment (only `spec.env`), in its own process group so a
/// timeout terminates the group with bounded waits. An unkillable child requires worker recovery.
pub fn run(spec: &Spec<'_>) -> Result<Outcome> {
    run_cancellable(spec, &Cancellation::default())
}

pub fn run_cancellable(spec: &Spec<'_>, cancellation: &Cancellation) -> Result<Outcome> {
    cancellation.check()?;
    let stdout = File::create(spec.stdout_path).map_err(io_err(spec.stdout_path))?;
    let stderr = File::create(spec.stderr_path).map_err(io_err(spec.stderr_path))?;
    let started_at = Utc::now();
    let started = Instant::now();

    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(spec.cwd)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ok(Outcome {
                exit_code: None,
                timed_out: false,
                spawn_error: Some(e.to_string()),
                started_at,
                finished_at: Utc::now(),
            });
        }
    };
    let pgid = Pid::from_raw(child.id() as i32);
    let mut stdin = child.stdin.take();
    let mut pending_input = spec.stdin.unwrap_or_default();
    if let Some(pipe) = &stdin
        && let Err(error) = fcntl(pipe, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
    {
        let stopped = terminate(
            || child.try_wait(),
            |signal| {
                let _ = killpg(pgid, signal);
            },
            Duration::ZERO,
            KILL_GRACE,
        );
        if !matches!(stopped, Ok(Some(_))) {
            return Err(WorkerError::UnstoppableProcess {
                program: spec.program.to_owned(),
                pid: child.id(),
            });
        }
        return Err(WorkerError::Invalid(format!(
            "set nonblocking stdin: {error}"
        )));
    }
    loop {
        let cancelled = cancellation.is_cancelled();
        // Feed stdin incrementally: an agent that never reads it must still be cancellable.
        if let Some(pipe) = &mut stdin {
            match pipe.write(pending_input) {
                Ok(written) => pending_input = &pending_input[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => pending_input = &[],
            }
            if pending_input.is_empty() {
                stdin = None;
            }
        }
        if let Some(status) = child.try_wait().map_err(io_err(spec.program))? {
            if cancelled {
                let _ = killpg(pgid, Signal::SIGKILL);
                return Err(WorkerError::Cancelled);
            }
            return Ok(Outcome {
                exit_code: status.code(),
                timed_out: false,
                spawn_error: None,
                started_at,
                finished_at: Utc::now(),
            });
        }
        if cancelled || started.elapsed() >= spec.timeout {
            tracing::warn!(program = %spec.program.display(), cancelled, "terminating process group");
            drop(stdin.take());
            let stopped = terminate(
                || child.try_wait(),
                |signal| {
                    let _ = killpg(pgid, signal);
                },
                TERM_GRACE,
                KILL_GRACE,
            );
            let status = match stopped {
                Ok(Some(status)) => status,
                _ => {
                    return Err(WorkerError::UnstoppableProcess {
                        program: spec.program.to_owned(),
                        pid: child.id(),
                    });
                }
            };
            if cancelled {
                return Err(WorkerError::Cancelled);
            }
            return Ok(Outcome {
                exit_code: status.code(),
                timed_out: true,
                spawn_error: None,
                started_at,
                finished_at: Utc::now(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn unkillable_child_has_a_bounded_termination_wait() {
        let mut signals = Vec::new();
        let started = Instant::now();
        let status = terminate(
            || Ok(None),
            |signal| signals.push(signal),
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(status.is_none());
        assert_eq!(signals, [Signal::SIGTERM, Signal::SIGKILL]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn termination_reaps_a_child_that_exits_only_after_sigkill() {
        let killed = std::cell::Cell::new(false);
        let status = terminate(
            || Ok(killed.get().then(|| ExitStatus::from_raw(9))),
            |signal| {
                killed.set(signal == Signal::SIGKILL);
            },
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap()
        .unwrap();
        assert_eq!(status.signal(), Some(9));
    }

    #[test]
    fn descendants_are_killed_even_when_the_parent_exits_on_sigterm() {
        let mut signals = Vec::new();
        let status = terminate(
            || Ok(Some(ExitStatus::from_raw(0))),
            |signal| signals.push(signal),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
        assert!(status.success());
        assert_eq!(signals, [Signal::SIGTERM, Signal::SIGKILL]);
    }
}
