// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! `git bundle` plumbing via the git binary. Arguments are passed as an argument vector; nothing is
//! interpolated into a shell.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::{GitError, Result};

/// Keeps a hanging bundle from wedging a worker or controller forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn run_git(repo_dir: &Path, args: &[&str], timeout: Option<Duration>) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_dir).args(args);
    // No prompts, no user config surprises, no hooks with side effects.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C");
    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
    let output = output_within(&mut command, repo_dir, timeout)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Command {
            command: std::iter::once("git".to_owned())
                .chain(["-C".to_owned(), repo_dir.display().to_string()])
                .chain(args.iter().map(|a| (*a).to_owned()))
                .collect(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Runs in its own process group, so that a timeout also stops Git's helpers such as `index-pack`.
fn output_within(command: &mut Command, repo_dir: &Path, timeout: Duration) -> Result<Output> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(crate::io_err(repo_dir))?;
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(crate::io_err(repo_dir))? {
            break status;
        }
        if Instant::now() >= deadline {
            let group = Pid::from_raw(child.id() as i32);
            let _ = killpg(group, Signal::SIGKILL);
            let _ = child.wait();
            return Err(GitError::Invalid(format!(
                "git timed out after {} seconds",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn read_in_background(stream: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = vec![];
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Writes a bundle containing the given revision arguments, for example `refs/heads/main` for a
/// full snapshot or `<base>..refs/heads/main` for a series with `<base>` as prerequisite.
pub fn create(repo_dir: &Path, out_path: &Path, rev_args: &[&str]) -> Result<()> {
    let out = out_path
        .to_str()
        .ok_or_else(|| GitError::Invalid("bundle path is not UTF-8".into()))?;
    let mut args = vec!["bundle", "create", "--quiet", out];
    args.extend_from_slice(rev_args);
    run_git(repo_dir, &args, None)?;
    Ok(())
}

/// Checks that the bundle is well-formed and that its prerequisites exist in `repo_dir`.
pub fn verify(repo_dir: &Path, bundle_path: &Path, timeout: Option<Duration>) -> Result<()> {
    let bundle = bundle_path
        .to_str()
        .ok_or_else(|| GitError::Invalid("bundle path is not UTF-8".into()))?;
    run_git(repo_dir, &["bundle", "verify", "--quiet", bundle], timeout)?;
    Ok(())
}

/// Fetches refs from a bundle into an existing repository, mapping them with `refspecs`.
pub fn fetch(
    repo_dir: &Path,
    bundle_path: &Path,
    refspecs: &[&str],
    timeout: Option<Duration>,
) -> Result<()> {
    let bundle = bundle_path
        .to_str()
        .ok_or_else(|| GitError::Invalid("bundle path is not UTF-8".into()))?;
    let mut args = vec!["fetch", "--quiet", "--no-tags", bundle];
    args.extend_from_slice(refspecs);
    run_git(repo_dir, &args, timeout)?;
    Ok(())
}

/// Clones a bundle into a fresh directory without checking anything out and without leaving a
/// remote behind. Callers create the branch and checkout afterwards.
pub fn clone_no_checkout(bundle_path: &Path, dest: &Path) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| GitError::Invalid("clone destination has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
    let bundle = bundle_path
        .to_str()
        .ok_or_else(|| GitError::Invalid("bundle path is not UTF-8".into()))?;
    let dest_str = dest
        .to_str()
        .ok_or_else(|| GitError::Invalid("clone destination is not UTF-8".into()))?;
    run_git(
        parent,
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            "--no-tags",
            bundle,
            dest_str,
        ],
        None,
    )?;
    run_git(dest, &["remote", "remove", "origin"], None)?;
    Ok(())
}
