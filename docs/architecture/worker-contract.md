<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Worker contract and execution

The controller sends immutable data; the worker returns immutable data and Git objects. Rust types in
[contract](../../crates/contract/src/lib.rs) are authoritative. Export current JSON Schemas with:

```sh
artidezi schema job
artidezi schema result
artidezi schema attempt
```

| Artifact | Producer → consumer | Purpose |
| --- | --- | --- |
| `job.json` | Controller → supervisor | Attempt identity, profile, limits, policy, repositories and artifact references |
| `task.md` | Controller → harness | Rendered project context, instructions and result rules |
| Repository bundles | Controller → supervisor | Exact base snapshots, optional source/previous inputs |
| `result.json` | Agent → controller through supervisor | Advisory verdict, summary, evidence, questions and changed-repository declarations |
| `attempt.json` | Supervisor → controller | Harness facts, collections, checks, logs and artifact index |
| Series bundles/patches/logs | Supervisor → controller | Candidate changes and supporting evidence |

New jobs carry schema version 2; stored version 1 jobs remain supported. Version 2 permits a revision
without `previous_attempt` artifacts when the prior execution returned no record. Version 1 retains
its requirement for previous artifacts on revisions. Result and attempt records still use version 1.
Workers advertise support for both job versions; the controller rejects incompatible workers before
leasing work. Upgrade workers alongside the controller.

The prompt template version is recorded separately in the job. It distinguishes globally enforced
rules, trusted project-wide `agent_instructions`, repository context and untrusted task/previous-
attempt text. For opted-in incoming-change handlers it also renders the frozen, structured CI result
snapshot and selected, bounded failed-job excerpts as untrusted forge data. Project instructions are
frozen in the resolved policy and cannot
expand structured repository access or effect permissions.
Changing a prompt does not silently revise
a stored task or require changing the wire schema. An optional additive config field likewise differs
from a worker contract change. If semantics cease to
be compatible, version the affected contract and its reader/dispatcher support explicitly.

## Preparing an attempt

The [daemon](../../crates/worker/src/daemon.rs) downloads the stored job bytes and authorized artifacts,
checks their identities, and runs one attempt at a time. Heartbeats cover downloads, execution,
uploads and reporting. A fresh
attempt directory, `<workspaces_root>/<attempt_id>/`, contains `task/` documents, `repos/`
checkouts, a fresh `home/`, a `tmp/` scratch directory and `out/` logs and results. `home/`, `tmp/`
and `out/` are private to the worker user (mode `0700`).

Setup recipes, the harness and verification recipes run with a cleared environment: `PATH` from the
supervisor, plus `HOME` and the `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_DATA_HOME` directories
under it, `TMPDIR` set to `tmp/`, `ARTIDEZI_WORKSPACE`, `ARTIDEZI_TASK_DOCUMENT`,
`ARTIDEZI_RESULT_PATH`, `ARTIDEZI_ATTEMPT_ID`, a fixed Git identity, `GIT_TERMINAL_PROMPT=0` and
`LC_ALL=C.UTF-8`. Adapters add their own variables, such as `CODEX_HOME`. Temporary files from
tools that honor `TMPDIR` therefore land on the workspaces disk rather than in the host's `/tmp`,
which may be a small `tmpfs`, and disappear with the workspace. `TMP` and `TEMP` are not set; tools
that ignore `TMPDIR`, such as the JVM's default `java.io.tmpdir`, still use `/tmp`.
The worker credential root is separate; the selected harness can copy required model login material
into the attempt home. Codex may rotate a ChatGPT refresh token while it runs. After the process
exits, the worker validates a changed `auth.json`, requires the same authentication mode and account,
checks that the durable source did not change concurrently, and replaces it atomically. This also
runs after cancellation or timeout. Persistence failures are recorded in the worker journal and
harness stderr without invalidating an otherwise complete model result. Successful acknowledged work
removes that attempt-local workspace. Any other workspace is retained for debugging without its
`home/` (credential copies) and `tmp/` (scratch), unless the worker is quarantined.

The daemon checks the lease before execution, while working (every one-third lease duration,
capped at 30 seconds), and before uploading. Heartbeat requests have a 10-second timeout separate
from artifact transfers. An authenticated heartbeat conflict means the attempt is no longer leased:
the worker stops its managed process group, skips subsequent setup/harness/verification stages and
retains the partial workspace locally instead of uploading a result. Network failures and other HTTP
errors do not masquerade as explicit revocation. If renewal remains unconfirmed for a full lease
period, the supervisor cancels managed execution itself; the controller enforces upload authority.
Lease, presence and heartbeat requests have a 10-second timeout; transfers and final result processing
retain their longer timeout. Codex/Pi version probes have a 10-second execution limit and a bounded
5-second kill wait, with file-based output capture to avoid inherited-pipe deadlocks. An unkillable
probe quarantines the worker, including when it occurs before leasing.

Termination sends SIGTERM, then SIGKILL to the process group after at most 10 seconds, and waits
at most another 5 seconds for the child to exit. If the child remains unresponsive (for example,
in an uninterruptible kernel wait), the daemon stops heartbeats, retains the workspace and inputs,
and writes `worker-quarantine.json` in its workspace root. The daemon refuses further leases;
the marker also prevents a supervisor restart from reusing the unhealthy environment. Recover
the environment, remove the marker and restart the supervisor before accepting more work.
An unresponsive harness's attempt-local credential is retained without copying it back while
the process may still be changing it.

The controller independently caps lease renewal at the initial lease time plus the frozen harness
wall-clock limit, every setup and verification recipe timeout, 15 seconds of termination allowance
per command, and 30 minutes for transfers, preparation and collection. Configuration changes and
controller restarts cannot extend this deadline. Expired leases cannot be revived by heartbeats;
worker downloads, uploads and final records are refused after lease expiry or the execution
deadline, even before maintenance marks the attempt lost. A separate local recovery loop expires
leases and can queue bounded fresh execution for an explicitly opted-in pool; see
[recovery constraints](lifecycle.md#recovery-constraints). Partial output is never published.

The `/worker/v1/status` endpoint reports supervisor presence and local quarantine without
granting publication rights. Lease polls and successful heartbeats also update last contact. Registry observations are supervisor claims, not independent
progress/health attestation. Scheduling drain and cooldown are owned by the controller and cannot
be cleared by worker presence reports. Multiple workers must use unique IDs and independent
workspace/credential roots. Concurrent Codex workers need separately authenticated sessions;
copyback conflict checks do not serialize refreshes of copies of the same session.

Input is fed without blocking cancellation when a process does not read stdin. This is cooperative worker
management, not isolation: independently daemonized processes and Docker containers started through
an external Docker daemon are outside that process group. Their lifecycle belongs to the worker
environment; ArtiDezi does not stop arbitrary containers on a shared host. Interrupted logs remain
local and are not completed attempt evidence in the UI.

[Workspace preparation](../../crates/git/src/workspace.rs) imports supplied Git bundles and creates
checkouts on the configured branches at their recorded base commits, with no forge remotes. An input
ref is additional history: source PRs appear at `refs/artidezi/inputs/source-pr`; checkout HEAD remains
the target base. No administrator workstation checkout is mounted merely because a prompt names it.
Read-only siblings supply examples and test context. Previous manual attempts can supply their result,
record and commit series for a fresh revision session.

## Execution and collection

[The supervisor](../../crates/worker/src/attempt.rs) runs configured setup recipes, invokes the harness,
collects repository state and runs applicable verification recipes. Recipes specify an argument
vector, relative working directory and timeout. The project owns those commands. Runtime commands
are not executed on the controller.

The [Codex adapter](../../crates/harness/src/codex.rs) and [Pi adapter](../../crates/harness/src/pi.rs)
support the full-access worker-boundary path; strict harness sandbox mode is rejected. Pi uses its
OpenAI-compatible Chat Completions provider configuration, so any endpoint implementing that API can
be selected through `model_endpoints` without a Pi-specific provider integration. Profiles supply
model/harness settings, and the adapters build fresh noninteractive invocations. This code does not
provision the worker VM, install its Docker daemon or enforce an external network firewall. Other modes
represented in types may not have runtime implementations. Consult adapter code before claiming support
for a new auth/execution mode.
If Codex exits after an HTTP 401 before any agent message or tool call, the supervisor may rerun
that invocation twice with 5- and 20-second waits. Unknown stream events, an agent action, a result
file, cancellation or the job's overall harness wall-clock limit prevent replay. Earlier streams
are retained as private attempt artifacts. Other harness failures remain held for human review.

The supervisor collects committed linear series above the recorded bases. It reports dirty trees,
branch/base changes, read-only violations, size/count limit problems and per-commit metadata.
A `proposal` must match actual changed writable repositories. `approve_untouched`, `needs_human` and
`failed` cannot be used to smuggle a changed series through the result contract.

Verification runs for non-empty valid writable series after the harness finishes. Records capture
command, directory, exit status, timing, timeout/skip/truncation facts and output artifact references.
An unchanged assessment triggers no automatic recipe execution. An agent may run checks itself;
those claims belong in evidence and must not be confused with supervisor-recorded verification.
Ad hoc files are not automatically uploaded, and normal worker cleanup removes the attempt
workspace. Evidence should record commands, outcomes and limits with concise supporting transcript
output, rather than referring to temporary files as if they were stored log artifacts.
For a missing result document, the task UI derives a short failure diagnostic from the stored
attempt status and exit code. It may classify a known Codex refresh-token rejection from a small,
integrity-checked stderr artifact, or an HTTP 401 authentication rejection from a bounded,
integrity-checked Codex JSON stdout stream. It displays only fixed categories, never log text inline.
This is an operator hint from worker evidence, not an independent proof of the failure cause.

## Controller validation

[Ingestion](../../crates/orchestrator/src/ingest.rs) compares the response to the exact stored job and
validates every declared repository and artifact. It inspects imported Git objects itself, including
base ancestry, linear series, paths, limits and commit metadata. Verification records must match the
configured steps and candidate head; failure in one required repository blocks the proposed delivery.

The controller reconstructs publishable commits itself; agent author identity and result text do not
grant permissions. A well-formed result is still untrusted evidence. A compromised worker can forge
its execution records and model output, so this is not an independent test runner or a proof that
the candidate is safe. See [the trust model](security-and-storage.md).
