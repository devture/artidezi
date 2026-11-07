<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Tasks, assessments and review

## Find and read tasks

Tasks have stable, instance-wide numbers such as **Task #234**. Browser links use `/tasks/234`;
`/tasks/tsk_…` links also work. Internal IDs remain the identifiers used by jobs, CLI/API
commands and forge branches.

The Tasks page combines a searchable **All projects** selector, text search and status filters.
Search matches instructions, project names and either form of task ID. Each status count reflects
the selected project and search. Pagination shows 50 tasks per page and retains all filters.
Times follow your **Account** time zone.
When a worker reports token use, task rows and details show input and output tokens.

The Tasks badge in the navigation counts tasks awaiting review or human attention. The Incoming changes badge counts open, unpaused tracked changes. Both follow your project access.

Agent names link to **Agent profiles**, which shows current configuration; existing tasks keep their frozen settings.

Task instructions and results use the main pane. Instructions support Markdown headings, lists,
links, tables and code blocks; embedded HTML is shown as text and images become links. Long
instructions show an excerpt with a **Read full instructions** button that expands the complete text
inline; **Show less** restores the excerpt. Without JavaScript, the full instructions stay visible.
The latest attempt is selected by default; numbered links select older attempts. Each card separates
the summary, checks, patches and
agent logs. Every check shows its command, repository, exit status and adjacent output links.
When an attempt has no result document, **Orchestrator validation** also shows the recorded harness
exit or timeout beside that finding. A recognized Codex refresh-token or HTTP 401 authentication
rejection is shown as a fixed diagnostic category; the page does not copy harness log text into the
finding. Anyone who can read the project can open the complete stored harness logs and transcripts,
so treat them as sensitive as the project itself. Line counts describe stored logs, which may have been truncated during collection.
Large or unavailable logs show stored size instead of an unverified count. Patch links open a
formatted diff. **Raw patch** opens the original text; large previews may be truncated without
changing the stored artifact.

Feedback is available on the latest result. **Activity** lists what happened to the task.

## Submit manual work

Open **New task**, select a project and an allowed profile/delivery mode, then describe the outcome
and useful acceptance checks. The project determines which repositories and commands are available.
Give the worker enough context to act without workstation paths, hidden deployment files or tokens.
Delivery choices outside the selected project’s policy remain disabled.

A useful request identifies the intended behavior, compatibility constraints and how to verify it.
For example: “Add configurable HTTP request timeouts; preserve the current default and test both
successful requests and timeouts.” A project configured for a paired migration can ask: “Add an iterator
API to the library and adapt the included application; preserve the old API until consumers can migrate,
and run the library tests plus the application's integration checks.”

Manual delivery modes are bounded by project policy:

| Mode | Behavior |
| --- | --- |
| `merge_request` | Publish proposals for review; track them until merged |
| `merge_request_with_automerge` | Publish proposals and use the configured forge/check gates for merging |
| `direct_push_fast_forward` | Push an accepted series directly if the target still matches; requires one writable repository |

Use ordinary merge-request delivery while establishing trust in a workflow. A task instruction cannot
turn on a delivery mode, grant a new repository or authorize tagging outside the configured policy.

## Read the result

The task page shows status, attempts, a summary, checks, logs/evidence and any published proposals.
`needs_human` can mean a question, a failed check, rejected output or a publication conflict. Read
the recorded reason rather than treating every such task as a model failure.

Checks come from the project's repository recipes. The worker supervisor runs setup commands before
the agent and verification commands after it. The orchestrator validates their records and blocks
publication when required checks fail; it does not execute repository commands itself. A command can
fail without printing anything, so **0 lines** is different from **Not captured** or **Unavailable**.
Use its exit status and command to interpret empty output. Truncated output is marked explicitly.

Evidence has different strengths. Agent-written claims are not enforced checks. Supervisor records
come from the same worker environment as the agent, not a separate trusted verifier. Inspect the actual
patch, its exact base/head, relevant tests and any uncertainty before deciding to merge.

For multi-repository work, inspect all proposals and their combined tests. A passing combined fixture
does not promise that every deployment can tolerate merging the PRs in either order. A task becomes
done after its required publication lifecycle completes; partial publication failure needs attention.

## Revise or close

For a pool with [automatic lost-execution recovery](operations.md#lost-execution-recovery-and-multiple-workers)
enabled, a lost attempt can be followed by a fresh queued attempt automatically. Earlier loss and
replacement linkage remain in the task's history. This preserves the frozen inputs and does not
resume the old session. Exhausted retries and any task with an accepted record/publication history
require human attention. Recognized temporary model failures can also retry automatically when enabled for the pool; the
attempt card shows the scheduled time and the earlier error. Manual feedback remains a separate
revision workflow.

On a manual task, feedback starts a fresh agent session on the current target branches. It receives
the original task and new feedback, plus the last attempt's result, execution record and commit bundles
when available. It re-applies relevant work; the old agent conversation and checkout are not resumed.
If a worker was lost before uploading its record, retry still includes the task and feedback, with an
explicit note that no previous execution record is available. Earlier attempts remain in the UI but
are not all included in the new job; repeat any older constraints that still matter in your feedback.

A revision keeps the task and updates ArtiDezi's existing proposal branch, with ownership and head
checks. It does not overwrite unrelated forge changes. Completed/closed manual tasks stay terminal.

**Close task** asks for confirmation before closing, using a separate page if JavaScript is unavailable.
Closing stops further task delivery and cancels active queued/leased work in the controller. Existing
forge PRs and audit history remain. Workers stop their managed processes when the next
heartbeat reports cancellation, skip later checks/uploads and preserve partial files locally.
Heartbeats run at intervals of at most 30 seconds, with bounded request and termination time.
Detached services and Docker containers
require cleanup by the worker environment; see the [worker contract](../architecture/worker-contract.md).

## Evaluate incoming changes

[Discovery](pr-discovery.md) creates a source-linked task and gives the worker the exact source PR
head alongside target and context snapshots. The default **dry-run mode** retains all results locally:

- `approve_untouched`: the agent considers the original update acceptable as-is; no merge or approval.
- `proposal`: a complete candidate adaptation is retained and checked, with no new forge PR.
- `needs_human` or `failed`: further automatic work is held for human attention.

A dry-run task marked done means its evaluation was recorded. It appears under **Completed**, like
other done tasks. The **Dry run** badge and status tooltip explain that nothing was published.
Unchanged inputs do not automatically run the project's verification recipes; inspect
what actually ran. Source movement/closure during execution makes a result obsolete.

With an explicit `mode: publish` handler, unchanged assessments post COMMENT reviews by default
and adaptations open human-reviewed PRs. An explicit `safe_verdict_action` can approve or merge
a unchanged GitHub update; see [source delivery policy](github-enrollment.md#approve-or-merge-safe-updates).
Eligible automatic delivery with pending checks shows **Waiting for CI**, also available as a task
filter. It continues automatically when checks pass and the assessment is still current. No worker
stays occupied, and no new evaluation is created. Close the task or pause its incoming source to stop it.
Publication failures expose **Retry publication**, which reuses the valid
result and reconciles existing effects without another worker. See the
[publication guide](github-enrollment.md#enable-publication) for scope and limits.

If the source HEAD changes before publication starts, ArtiDezi closes the obsolete assessment and
schedules a fresh evaluation within the existing source budget. The task links to that follow-up.
A retry request that never started publication does not prevent this recovery. Human decisions,
explicit source pauses and possible existing forge effects retain their holds. For an invalid or
lost execution requiring manual recovery, **Retry evaluation** starts a fresh evaluation without
requiring a comment.

Use **Evaluate again** for fresh source-aware feedback. It creates a new linked task against
the current head and preserves earlier completed tasks. It includes your follow-up instructions with
the current source PR context; it does not inherit the previous assessment’s result or commits. **Incoming changes** exposes scan, pause and resume.
Its rows keep the evaluation status at the far right, after task and evaluation controls.
Explicit resume renews the source run budget and can evaluate the same head again, so use it when
another model invocation is intended. Resolve open adaptation proposals before reevaluating or
resuming; neither action rewrites existing source-task proposals. PR comments are not interpreted as commands.

## CLI and API

The CLI supports `task create`, `list`, `show`, `revise`, `close` and `tick`. Use `artidezi task --help`
and the relevant subcommand's help for flags. Set `ARTIDEZI_URL` and `ARTIDEZI_ADMIN_TOKEN_FILE` to
select the instance without pasting a bearer token into a command.

The admin API exposes the same task workflow plus discovery routes. Worker tokens and UI cookies
cannot authorize admin requests. API shapes are in [the server routes](../../crates/server/src/lib.rs)
and [admin handlers](../../crates/server/src/admin.rs). Admin API `actor`/`created_by` strings are
caller-supplied audit labels, not authenticated users. The web UI records the [signed-in account](operations.md#enabling-the-web-ui).
