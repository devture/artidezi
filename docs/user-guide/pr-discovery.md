<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Incoming changes

Discovery creates tasks automatically for selected incoming pull/merge requests. A project chooses
which repositories and authors to watch, how often to scan, and what instructions to give the agent.
For example, an agent can evaluate a dependency update and prepare compatibility changes if needed.
The source request is tracked across revisions; each evaluation is an ordinary task with its own
result, patches and check records. Manual tasks work without discovery.

An optional project-level `agent_instructions` block applies shared maintenance guidance to every
task. Handler `instructions` are the next, narrower layer for one class of incoming change; the
source title and description are appended as untrusted task data. Repository `AGENTS.md` files may
provide checkout-local guidance to harnesses that support them. Keep broadly reusable behavior at
the project level and the dependency- or workflow-specific assessment procedure in the handler.

**Dry-run mode keeps results inside ArtiDezi.** It is the default for each handler and is useful for
enrollment and debugging. Opt-in **publish** mode posts informational reviews and, by default, human-reviewed
adaptation PRs. Publish handlers can additionally opt into [safe source approval or
merge](#approve-or-merge-safe-updates); dry runs have no forge effects. Tagging and release
operations remain unavailable. See [GitHub publication](github-enrollment.md#enable-publication)
for activation and recovery on GitHub.

Discovery is disabled unless a project explicitly defines `pull_requests`. Add this alongside that
project's existing repositories and policy (replace `application` with a writable repository ID
and `dependency-bot` with the updater's forge username):

```yaml
pull_requests:
  poll_interval_seconds: 600
  settle_seconds: 30
  max_runs_per_pull_request: 3
  max_active_tasks: 1
  repositories: [application]
  handlers:
    - id: dependency-updates
      authors: [dependency-bot]
      action: assess
      mode: dry_run  # default; publish requires the project publication effect
      wait_for_checks_to_finish: true  # default
      require_successful_checks: false
      target_conflicts: wait_for_rebase  # default; see below
      ci_context:
        results: true
      instructions: >-
        Assess the dependency update and compatibility with this application's usage.
        Prepare an adaptation only when the original update needs changes.
```

Authors are matched against forge identities, ignoring case. A handler names them with exactly one
of `authors` (a list of logins), `author_regex` or an explicit `catch_all: true`. `author_regex` uses
Rust regular-expression syntax and must match the whole login, as if wrapped in `^(?:...)$`: for
example `'[a-z0-9-]+\[bot\]'` matches every GitHub App bot but not `someone[bot]-fork`. A pattern that
matches an empty login is invalid.

A handler with `authors` or `author_regex` may additionally set `title_regex`; both the author and
complete forge title must then match. Unlike `author_regex`, the title expression is not anchored for
you, so anchor it with `^` and `$` when the complete title is the intended contract. Title matching
is unavailable on a catch-all. For example:

```yaml
- id: command-image
  authors: [dependency-bot]
  title_regex: '^Update registry\.example\.org/tools/command image to v[0-9]+\.[0-9]+\.[0-9]+$'
  agent_profile: maintenance-low
  action: assess
  require_successful_checks: true
  instructions: Assess the command-image update and its actual use in this repository.
```

The first matching handler owns the PR, including while waiting for CI or human input. This permits
several ordered rules for one updater identity, with distinct instructions and delivery policies.
An optional `agent_profile` selects one of the project's `allowed_agent_profiles` for that handler;
omitting it uses the project default. The resolved profile is frozen into each source task.
Unmatched PRs are ignored. `action: ignore` excludes matching PRs. An optional catch-all must
explicitly set `catch_all: true`, omit `authors`, `author_regex` and `title_regex`, and appear last.
A handler without an author selector is invalid; there is no built-in catch-all and no automatic
repository discovery.
Only the named repositories and their configured target branches are watched. Context repositories
are not automatically watched; a remote/branch can have only one discovery project owner.

`ci_context.results: true` is an independent, optional agent-context setting for GitHub and Forgejo
sources. It freezes every visible commit-status and latest check-run name/state reported for the
exact source head into the task. Forgejo has no check runs: Forgejo Actions jobs and external CI
both appear as commit statuses, such as `ci / test (pull_request)`. The prompt marks names and states as untrusted forge data, tells the agent not to
rerun successful checks merely to duplicate CI. The snapshot helps
an agent focus on an actual failure when `require_successful_checks: false`; it does not change
eligibility, prove required checks or replace the controller's fresh publication gate. Omit the
setting (the default) where exposing CI metadata is undesirable.

GitHub Actions and Forgejo Actions job logs require two separate opt-ins: set
`pull_requests.ci_log_context.permitted: true` for the project and `ci_context.logs: true` on
the assessing handler. The handler must also set `ci_context.results: true`. No job names need to
be configured; failed jobs are discovered from the PR's exact source commit:

```yaml
pull_requests:
  ci_log_context:
    permitted: true
  handlers:
    - id: selected-update
      # Other required handler fields omitted here.
      ci_context:
        results: true
        logs: true
```

Collection is off by default, even when the App has Actions read permission. The result overview
includes passing, failing and pending checks whenever `results: true`, even if no log is collected.
At most two completed failed jobs from the exact source SHA are selected: on GitHub, Actions jobs
matching failed check runs; on Forgejo, the Actions jobs that failed commit statuses link to.
Forgejo selects the job by that link rather than by name, since several workflows can share a job
name, and reads the log of the job's latest attempt. Successful job logs and logs from other CI
providers are not collected. Each
sanitized excerpt is at most 3 KiB of text
inside a 4 KiB per-job and 8 KiB per-task rendered limit; raw downloads over 8 MiB are rejected.
The excerpt keeps a strong failure diagnostic when one appears within the final 64 KiB of
sanitized log text, plus the end of the log, and marks omitted bytes. For Python tracebacks,
it keeps the exception endings of the last two tracebacks so a wrapper error need not hide the
original cause. It can still miss a cause
outside that window or one expressed without a recognized diagnostic marker. Missing, pending,
inaccessible or oversized logs appear as unavailable evidence while the structured CI snapshot
remains. The prompt labels log text as untrusted and forbids copying it into a public assessment.
These controls do not prove that logs are free of secrets: selected text reaches the model worker,
private controller artifacts, task history, backups and administrators. Source-CI evidence has no
automatic expiry.
Enable this only for jobs whose output is acceptable in those places. On GitHub a separate
one-repository Actions-read App token is used; Workflows write does not grant log access. On
Forgejo the bot's token reads the log, so it needs read access to the repository. This snapshot never
replaces fresh publication checks.

The scheduler scans due projects; **Incoming changes → Scan now** uses the same matching and eligibility
rules immediately. A new eligible PR or changed source head can queue an assessment. Description
edits and comments do not activate work. A title-scoped task freezes its matched title; retitling it
stops publication, and a later scan treats a no-longer-matching title as a routing change requiring
explicit review. An unchanged head does not repeat after a successful evaluation or restart. The
settle window coalesces bursts. Known unresolved sources are revisited directly when their settle
deadline arrives; a still-running check or draft PR gets another direct read every two minutes.
These follow-ups do not list every watched repository. The configured poll interval still governs
discovery of new PRs and routine detection of changed, closed and reopened sources. By default,
`wait_for_checks_to_finish` waits for all visible commit statuses and check runs to reach a terminal
state. Passing and failing checks both permit an assessment; a failed check can help explain why an
adaptation is needed. If no checks are reported, the PR can be assessed after its settle window.
Setting `wait_for_checks_to_finish: false` permits assessment with running checks after settling.
`require_successful_checks` remains a separate publication freshness gate for handlers that use it;
automatic source approval or merge requires current successful checks. An all-skipped check set
never satisfies that source-action gate, and an empty one does so only for a handler that opts in
with [`safe_verdict_checks: passing_or_none`](#repositories-without-ci). One source has at most one active attempt;
project capacity bounds queued/running assessments. Budgets count queued evaluations, including
superseded ones but not [retries](#automatic-reevaluation). An exhausted budget pauses that source
until an explicit human action. Failed attempts and requests for human input wait for a person too,
but the change is [evaluated again](#automatic-reevaluation) by itself where that can help. A worker
pool can additionally [opt into immediate retries](operations.md#lost-execution-recovery-and-multiple-workers) of lost or transiently failed execution. Within one worker
attempt, a Codex HTTP 401 that occurs before agent work may receive two bounded retries.

### Automatic reevaluation

| What happened | What ArtiDezi does | Default limit |
| --- | --- | --- |
| A run broke, was lost, produced no acceptable result or reported `failed` | Evaluates again after an hour; until then the task shows **Waiting to retry** | once per change |
| The assessment reported a temporary blocker (`retry_later`), such as an image that isn't published yet | Evaluates again after 1, 3, 9 and 24 hours, telling the agent what blocked it; until then the task shows **Waiting to retry** | 4 times per change |
| A held assessment, and the change itself differs now | Evaluates the new revision after it settles | the run budget |
| A held assessment, and its target branch moved or the change was merely rebased | Evaluates again after an hour; until then it stays with you, in case you answer first | once per change |

```yaml
pull_requests:
  retries:                                        # these are the defaults
    retry_later:                                  # max: 0 turns it off; at most 10
      max: 4
      initial_delay_seconds: 3600
      multiplier: 3
      max_delay_seconds: 86400                    # at most 604800
    failed: {max: 1, initial_delay_seconds: 3600}        # 0 turns it off; at most 5
    target_change: {max: 1, initial_delay_seconds: 3600}
```

`retry_later` is the agent's verdict that something specific outside the change prevents a decision
for now and will clear by itself. It must say what. The agent doesn't choose times or counts, nothing
is posted to the forge while waiting, and a person is asked only when the retries run out. With
`max: 0`, agents aren't offered this outcome.

These evaluations don't count against `max_runs_per_pull_request`, use the current head, target, CI
and configuration, and pass the usual gates. Their counts start over when the change itself differs,
or when a person resumes it or gives feedback. “Merely rebased” means that Git proves the same paths, contents and
modes, as for [published adaptations](#conflicts-with-the-target-branch); anything unclear counts as
a different change and uses the run budget.

A held assessment is one that asks a person, failed, ran out of retries, or stopped while being delivered, for example
because the forge refused the merge or delivery retries ran out. Only a new head, not a moved
target, releases one that is merely out of date, such as a changed handler, missing checks or a
protected CI path.

Never evaluated again automatically: pauses by a person; while a reviewer requests changes, or after
someone edited, dismissed or deleted the assessment; open or foreign adaptation proposals; results
that broke a rule (paths, read-only repositories, forged records); and runs that ended because
the model provider refused credentials, permissions or quota, or because Claude Code reached its budget or turn limit. Holds from releases before this feature are adopted at startup where their
reason is certain, so they can recover without a person.

A failed run can have started work outside ArtiDezi's control. Raise `initial_delay_seconds` beyond
your longest run if an overlapping lost worker could interfere, or set `max: 0`.

### Automatic delivery retries

A temporary failure while a validated result is being delivered does not need a person. When the
forge or its Git server fails on its side, times out, drops the connection or rate-limits ArtiDezi,
or a restart interrupts delivery, the task shows **Waiting to retry** with the next time and the
cause. ArtiDezi then delivers the same result again, without another agent run and without charging
the run budget.

```yaml
pull_requests:
  retries:
    delivery:                      # these are the defaults
      max: 8                       # 0 asks a person right away; at most 20
      initial_delay_seconds: 120
      multiplier: 2
      max_delay_seconds: 3600
```

With the defaults, retries wait 2, 4, 8, 16 and 32 minutes and then an hour each, about four hours
in total. A rate limit that asks for a longer wait is respected. The limit counts per validated
result; a person's **Retry publication** starts it over. The same limits apply to merge-request
delivery of the project's manual tasks, and the defaults apply when the project has no
`pull_requests`. Setting `max: 0` also stops a retry that is already scheduled.

Every retry repeats all gates: source head, checks, reviews, pauses and current permissions. It looks
for what already happened before acting: an existing proposal branch, proposal, review or merge is
recognized, and a merge always names the assessed head. A proposal revision remembers what it
replaces before its first effect, and stops if someone changed that meanwhile. An approval whose response was lost is
posted again only while the forge shows no trace of it, so in rare cases the same commit can carry
two identical approvals. If the source moved meanwhile, the new head is evaluated within the run
budget instead; if it was closed or merged, the task closes.

Not retried: pauses, requested changes, rejected credentials or permissions, a merge the forge
refuses, edited, dismissed or deleted reviews of ArtiDezi, proposals that someone edited, merged or
closed, direct pushes and release tags. Waiting for a backoff that is already known uses no retry. Preparing an evaluation during an outage is tried again every two minutes without
stopping the change. A change that is rerouted while its delivery waits is stopped by the next retry.

### Conflicts with the target branch

A change can stop merging cleanly only because its target moved, for example when another update
edits a neighbouring line of the same file. Updaters such as Renovate rebase their own PRs in that
case, so an adaptation that only resolves the conflict duplicates their work and needs human review.
By default (`target_conflicts: wait_for_rebase`), ArtiDezi checks each eligible head before
evaluating it: after the settle window and CI wait, it merges the head into the current target
commit in memory, without running repository code or the model. When that merge conflicts, whatever caused it (including a new commit on the
change itself), no evaluation starts and nothing counts against the run budget. The change shows **Waiting for
rebase** in Incoming changes, with the reason, for example
“Conflicts with `main` in `justfile`; waiting for the author or updater to rebase”.

The rebased head is a new revision, so it is evaluated as usual and can be approved or merged
unchanged. Later scans do not repeat the merge while neither the head nor the target has moved, and
the change is not paused: if the updater never rebases (for example, rebasing is disabled), it
simply keeps waiting. To evaluate the conflicting revision anyway, use **Evaluate anyway** (the
same action as **Resume evaluations**); the agent may then prepare a conflict-resolved adaptation.
This applies to that one evaluation, not to later returns to the same revision. Feedback on an earlier task of the same
change also starts an evaluation regardless of conflicts. Set `target_conflicts: adapt` on a handler
to evaluate conflicting heads immediately, as releases before this setting did.

This check reports what a plain three-way merge into the target would leave unresolved: conflicting
edits, as well as cases such as one side deleting a file the other modified. Merge settings in
`.gitattributes` (such as `merge=union`) are not applied, so a file that the forge would merge with
such a driver can still be reported as conflicting; use **Evaluate anyway** or
`target_conflicts: adapt` for such repositories. Changes touching files over 16 MiB, and
branches with criss-cross merge history, are not merged on the controller; they are evaluated as usual. The
check can also miss conflicts that Git's directory-rename detection would report; such changes are
evaluated as before. A clean merge says nothing about whether the
result builds or behaves; that remains the agent's assessment. If the target moves
after an evaluation has started, the frozen input is assessed as recorded, and a source merge that
the forge then refuses holds the task as before.

A source rebase during a successful evaluation does not reuse the old verdict. If the only
changed source field is its head, the accepted result is an unchanged approval or an adaptation,
and publication has not been planned or attempted, the controller closes the obsolete task and
schedules a fresh evaluation of the new head. The normal settle window, CI wait, project capacity
and existing run budget still apply; superseding work does not refund or renew that budget.
This also applies to an accepted result waiting for CI before any publication activity.
When the source moves after an adaptation was published, ArtiDezi first checks whether it is only a
rebase carrying the exact same change onto its target. A proven equivalent change keeps the existing
proposals ready for review, with no comment, branch update, new evaluation or budget charge. Repeated
scans and restarts retain that decision. When those proposals merge, the task completes normally and
the rebased head does not trigger another evaluation. The activity log records the frozen and observed
heads and target commits used for the comparison; the assessment still belongs to its original input.
If fetching or comparison cannot prove equivalence, the controller closes its own proposal with a
comment naming the new head and evaluates that head the same way. It never closes a
proposal that someone else changed, merged or closed, and keeps the hold after an explicit pause.

After closing an obsolete adaptation, it also attempts to delete that task's recorded branch if
repository write permission still applies and no open proposal or pending publication uses it.
Deletion requires the branch to remain at the exact published commit; an absent branch is already
clean, and a moved branch is left alone. Cleanup outcomes appear in task activity. A cleanup error
does not prevent retiring the old task or evaluating the new source head. Equivalent rebases keep
their open branches. When an adaptation is merged, ArtiDezi also cleans up its recorded branch under
the same guards, even when other repositories still await review. Temporary cleanup failures retry
after delivery completes; see [proposal branch cleanup](tasks.md#proposal-branch-cleanup).
Historical branches and human-closed proposals are not swept.

Explicit pauses and exhausted budgets stay held; requests for human input and failed results follow
[automatic reevaluation](#automatic-reevaluation).
Unpublished results still follow the fresh-evaluation rules above, even for equivalent rebases.
Source reviews, approvals and automatic merges always require the exact assessed commit. Revising an
existing proposal also refuses a moved source, even when the open proposal was preserved.

Failed checks at the moment of automatic approval or merge hold the task as needing attention and
show the source as paused with “Current forge checks failed”. CI failures are often temporary (a
forge outage or flaky runner), so the controller rechecks that hold every two minutes without a
model run. When a re-run turns the same head green, delivery continues through the usual review,
freshness and policy gates, reusing an approval that was already posted. When the source moves to
a new head, the held task closes and the new head is evaluated after settling, within the existing
run budget. Checks that stay red keep the hold. A later explicit pause or a reviewer requesting
changes still needs a person.
A changed route still requires inspection. **Retry publication** continues to reuse the recorded result and never queues
a new model run; use **Resume evaluations** or task feedback to evaluate a held assessment right away.

The worker receives the exact PR commit at `refs/artidezi/inputs/source-pr`, alongside target-branch
and sibling snapshots, without forge credentials or remotes. An acceptable update returns the
advisory `approve_untouched` outcome. An adaptation returns a complete replacement commit series on
the target base, including the update itself. Its PR is titled after the original change, for
example `Adapt #3: Update example-library to v2` (`owner/name#3` when the adaptation is in another
repository), so it stays distinct from the PR it replaces. The normal verification recipes run
for changed writable repositories. Unchanged inputs do not automatically run those recipes; remote CI is an
eligibility wait by default when checks have started. It uses visible commit statuses and check
runs, not a configurable list of required checks or a guarantee of independent verification.
When CI result context is enabled, its point-in-time details are evidence for the agent rather than
another gate.

The UI calls the default mode **Dry run**; its stored/API delivery remains `shadow`. A `publish`
handler creates source tasks with `merge_request` delivery. Manual delivery defaults do not choose
incoming behavior. Dry-run completion means the result was recorded. Publication completion means
a review was posted, a configured source approval/merge completed, or adaptation proposals were
merged. The activity records show which action occurred. Changed, closed, retargeted or draft sources withhold
publication. ArtiDezi-authored PRs and `artidezi/` branches are excluded from automatic intake.

Open **Incoming changes** in the main navigation for an instance-wide view. Choose a project
(default: **All projects**), search by title/repository/project/number, then select **Open**, **Paused**
or **Closed**. Counts reflect the selected project and search; pagination preserves all filters
(25 changes per page). Searching `owner/repository #123` matches that exact source number. Each row
leads with `owner/repository #123 (source title)`. When forge links are available, the repository
opens its forge page while the number and title open the source request. **Task #N** opens the latest
evaluation. Each row shows
its evaluation mode, rather than a page-wide mode label. The
project card’s delivery badge is the default for manual tasks; incoming evaluations use their handler
mode, which also appears on the project card. Project cards link to the same page with that project selected.

The tabs track the source request: **Open** and **Closed** do not filter by evaluation outcome.
The badge describes the current stage of that source and its latest task:

- **Waiting**: eligibility, timing or capacity prevents an evaluation from starting; the reason appears below the source.
- **Waiting for rebase**: the latest revision [conflicts with its target](#conflicts-with-the-target-branch); it is evaluated after a rebase or an explicit resume.
- **In progress**: an evaluation task is queued or running.
- **Evaluated**: the latest observed source revision has a completed evaluation. Open its task for the verdict and evidence; the source request may still be open.
- **Ready for review**: published adaptations are awaiting human review, or their checks when the
  handler merges adaptations.
- **Waiting to retry**: a temporary failure interrupted delivery, which [continues by itself](#automatic-delivery-retries), or a run failed and is [repeated](#automatic-reevaluation); the next time appears below the source.
- **Needs attention**: the latest evaluation needs human input. A scheduled reevaluation shows its time below the source.
- **Paused**: automatic evaluations are held. A running evaluation can still finish and shows a separate pause badge.
- **Closed or merged**: the source request is closed and no longer evaluated.

A new source revision can return a completed item to Waiting or In progress. Completing a dry run
does not close or merge the source.

Browsing and automatic page refresh only read stored tracking state. **Scan now** scans the selected
project, or all configured discovery projects when **All projects** is selected by an instance
administrator; it can queue work. A project writer can scan only a selected writable project.
While a scan runs, the page shows a running message without blocking the list. When it finishes, the
page refreshes the list and reports how many evaluations were queued, along with any scan errors.
Search text and status tabs filter the displayed rows, not the scan's repository scope. Pause/resume
controls affect only their named source and return to the same filtered view. Closed sources retain
history without those controls. Projects or repositories no longer watched retain historical rows
marked **Discovery off**, without resume controls. These rows track source requests, not additional projects.

**Resume evaluations** renews the source's budget and permits another evaluation of the same head,
including one that waits for a rebase.
**Pause evaluations** cancels queued work and withholds pending publication; an already running attempt may finish, with further
automatic runs held. Task feedback creates a linked assessment of the current source head, retaining
completed tasks as history. PR comments are not interpreted as commands. Configuration changes do not
automatically rerun completed heads; explicitly resume or provide feedback when a new evaluation is wanted.

When a scan confirms that a source is closed or merged, queued and running assessments are cancelled.
On GitHub, routine scans use the complete open-PR listing to find new and changed sources; only
candidates need individual PR and check details. A missing previously open source is fetched
individually before closure is accepted.

Detection follows the project's poll interval; **Scan now** can detect it sooner. The next worker heartbeat stops managed execution. This also applies when evaluations were paused.
Previously published adaptations and completed evidence remain available for inspection.
Already closed sources with no active task are retained without an individual request on every
scan; a reopened source appears in the open-PR listing and is assessed under the usual rules.
Closed sources with unfinished tasks continue to be checked until those tasks settle.

When a scan observes a previously closed source reopen with a different head, it starts a fresh
evaluation lifecycle if the prior task and attempt are terminal and all adaptation proposals are
closed or merged. This renews the run budget, clears the latest-task link and any hold except a person's pause, and applies the
usual settling, matching, checks and capacity gates. Previous tasks and published adaptations retain
their history. An unchanged-head reopening or a head change while the source stays open does not
renew the budget or clear a hold, apart from [automatic reevaluation](#automatic-reevaluation). Unfinished work and open adaptations still require attention.
Both closure and reopening must be observed by discovery; otherwise, use **Resume evaluations** to
recover that source.

Authenticated API equivalents:

- `GET /admin/v1/discovery`: tracked sources, statuses, holds and budgets.
- `POST /admin/v1/discovery/scan` with `{"project":"project-name"}` (or `{}` for all enabled projects).
- `POST /admin/v1/discovery/{id}/pause` or `/resume` with `{"actor":"admin"}`.
- The existing task revision endpoint accepts human feedback and returns the new linked attempt.
- `POST /admin/v1/tasks/{id}/retry-publication` retries interrupted delivery without a new evaluation, right away.

Open adaptation proposals must be resolved before starting
a new evaluation, including via Resume or task feedback. Resume closes the controller's own proposals
for a source head that has since moved, unless equivalence preserves them. Retained proposals must
be merged or closed before a replacement evaluation can start. Retry publication first when only
delivery was interrupted.

Discovery uses the normal worker contract; see [its version notes](../architecture/worker-contract.md). Configurations without discovery keep manual-only behavior.

For internal scheduling and recovery behavior, see [PR discovery internals](../architecture/pr-discovery.md).

## Approve or merge safe updates

A publish handler's `safe_verdict_action` chooses what an accepted `approve_untouched` result may do
with the source PR, on GitHub and on Forgejo:

| Value | Action |
| --- | --- |
| `comment` (default) | Publish an informational review |
| `approve` | Submit an approving review on the exact assessed commit; another bot may then merge |
| `merge` | Submit that approval, then merge the exact assessed commit with a merge commit |

Both actions need `mode: publish`, the project's `publish_adaptation_merge_requests` effect and
writable source repositories. `merge` also needs
`effects.merge_source_merge_request_on_safe_verdict: true` (formerly
`merge_renovate_merge_request_on_safe_verdict`, which ArtiDezi still accepts; `artidezi schema
config` shows only the new name, so rename it when validating against that schema). A merge handler merges unchanged PRs
from exactly the authors it matches, whoever they are: a dependency bot such as `renovate[bot]` on
GitHub or `renovate-bot` on Forgejo, another automation account, or a person. Separate handlers give
different authors different actions:

```yaml
handlers:
  - id: dependency-updates
    authors: [renovate-bot]
    action: assess
    mode: publish
    safe_verdict_action: merge
    require_successful_checks: false
    instructions: >-
      Review compatibility and run relevant checks. Propose adaptations when needed;
      report uncertainty honestly and request input when evidence is insufficient.
  - id: contributor-changes
    authors: [contributor]
    action: assess
    mode: publish
    safe_verdict_action: comment
    require_successful_checks: false
    instructions: Review the change for correctness and fit with this repository.
```

Listing an author on a merge handler is the decision to merge their unchanged PRs without a person.
The author controls the change and its description, and can word both to persuade the reviewing
agent. Whether that is acceptable depends on who can open PRs as that login and on the repository,
so ArtiDezi leaves it to the operator instead of guessing from login shapes or repository
visibility.

An `author_regex` or `catch_all: true` on an approve or merge handler widens that decision to every
login it matches, including accounts created later. `catch_all` covers everyone who can open a PR
against the watched branch, which on a public repository includes anyone with an account, so
reserve it for repositories where only trusted people can open PRs. ArtiDezi's own PRs and
`artidezi/` branches are never taken in as sources, whatever the selector.

Right before approving and again before merging, the controller requires current successful
commit statuses on the assessed head, no reviewer whose latest decision requests changes, an
unchanged source and the same configuration. Pending checks wait for CI; failed checks hold the task
as described above. The agent's risk rating, evidence and questions appear in the review; they are
context, not an extra gate, and an agent that needs a decision returns `needs_human` instead.

### Repositories without CI

A repository whose forge reports no checks at all for a PR's head can never satisfy that gate, so
by default its unchanged PRs wait for a person. An approve or merge handler can accept that case
explicitly with `safe_verdict_checks`:

| Value | Approve or merge when the head's checks are |
| --- | --- |
| `passing` (default) | reported and successful |
| `passing_or_none` | reported and successful, or not reported at all |

```yaml
handlers:
  - id: dependency-updates
    authors: [renovate-bot]
    action: assess
    mode: publish
    safe_verdict_action: merge
    safe_verdict_checks: passing_or_none
    require_successful_checks: false
    instructions: >-
      This repository has no CI. Read the upstream changelog and the diff, and run the
      relevant tests yourself before deciding that the update needs no adaptation.
```

With `passing_or_none`, the agent's assessment is the only evidence behind the approval or merge:
nothing independent has built or tested the change. Ask the agent in the handler's instructions for
the checks you would otherwise expect from CI, and keep it to repositories where that is enough.
Running checks still wait and failed checks still hold the task. A set of checks that all reported
`skipped` or neutral still does not qualify: it means the repository has CI that chose not to run.
Freshness, author, review and authority gates are unchanged, and changing the setting invalidates
assessments made under the previous value.

The approving review then says that no forge checks were reported for the commit, and the
`assessment.approval_planned` and `source.merge_planned` events carry `"forge_checks": "none"`.
The setting needs `require_successful_checks: false`, which would otherwise refuse to publish the
assessment of a head without checks, and a `safe_verdict_action` of `approve` or `merge`.

On Forgejo, the merge request names the assessed commit (`head_commit_id`), so Forgejo itself
refuses the merge if the PR moved after the last check. The approval has no such guard. Configure
branch protection for your final policy: required status checks, required approvals, dismissal of
stale approvals, and whether the bot may merge at all. ArtiDezi does not change protection, use
"merge when checks succeed" or fall back to another merge style; a refused merge holds the task.
Forgejo statuses count as green only when every reported context succeeded or was skipped and at
least one succeeded. `warning` counts as a failure.

## Merge ArtiDezi's own adaptations

By default, an adaptation PR waits for a person to review and merge it, whatever the handler's
`safe_verdict_action`. A publish handler can instead set `adaptation_action: merge`, together with
the project effect `merge_adaptation_merge_requests: true`:

```yaml
effects:
  merge_source_merge_request_on_safe_verdict: true
  publish_adaptation_merge_requests: true
  merge_adaptation_merge_requests: true
pull_requests:
  handlers:
    - id: dependency-updates
      authors: [renovate-bot]
      action: assess
      mode: publish
      safe_verdict_action: merge
      adaptation_action: merge
      require_successful_checks: false
      instructions: >-
        Review compatibility and run relevant checks. Adapt the repository when the update needs it.
```

The two settings are independent: a handler can merge adaptations while only commenting on
unchanged updates, or the reverse. `adaptation_action: merge` needs `mode: publish`,
`draft_merge_requests: false` and every writable repository of the project on GitHub or Forgejo,
because an adaptation may change any of them.

The adaptation PR and the review on the original change say that ArtiDezi will merge the adaptation.
On each reconcile pass (the maintenance tick, or `POST /admin/v1/tasks/{task_id}/reconcile`), the
controller merges the task's adaptations with a merge commit once all of the following hold:

- every adaptation PR still has the head ArtiDezi published, is not a draft and has successful
  checks; pending checks keep it waiting, and absent or failed checks leave it to a person;
- nobody's latest review requests changes, on the adaptations or on the original change;
- the original change is still open at the assessed commit, not a draft, with the same author,
  branches and (for title-matched handlers) title; its source is not paused; and the handler and
  project still allow adaptation merges;
- the adaptation does not change protected CI paths (those already needed a person to publish them).

Merging gives the agent's own code the same standing as a person's merge, so enable it where
checks cover what matters: tests, linting, a deployment dry run. A merge the forge refuses, for
example because branch protection requires an approval, is recorded as `adaptation.merge_failed`
and not tried again for that head; the adaptation then waits for a person, as without this setting.
A merge that failed temporarily or was interrupted by a restart is tried again under the
[delivery retry limits](#automatic-delivery-retries), after checking whether it happened anyway. With several writable repositories, adaptations merge
one after another once all are ready, and the conditions above are checked again before each one; a
failure or change part-way leaves the remaining ones open. The original change is not merged or closed by
ArtiDezi: once the adaptation lands, the updater usually closes or rebases it.
