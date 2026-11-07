<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# PR discovery internals

[Configuration](../../crates/config/src/model.rs) adds optional `pull_requests` to a project.
[Validation](../../crates/config/src/validate.rs) requires watched writable repositories, explicit
polling/settle/limit settings and ordered handlers. Each handler has a stable ID, exactly one of an
author list or `catch_all: true`, an `assess`/`ignore` action, a check gate and instructions. A handler
may select an agent profile already allowed by its project; otherwise the project default applies.
GitHub assessment handlers may independently opt into a frozen structured CI-result snapshot.
Failed Actions job excerpts additionally require a project permit and handler opt-in; neither
context setting alters the check gate.
Explicit author handlers may additionally carry a validated `title_regex`, matched against the
complete forge title after the author match. A catch-all must be last and cannot carry a title
expression. There is no broader expression language or hidden default reviewer.

## Ownership and scheduling

The main maintenance loop calls [discovery](../../crates/orchestrator/src/discovery.rs). Each project's
next scan time is persisted before forge access, preventing rapid retry on process restart. Effective
scan granularity is bounded by the server maintenance tick (30 seconds by default); a shorter project
interval alone cannot create a faster timer. Forced scans bypass the interval only.
The maintenance tick also revisits known unresolved sources at persisted follow-up deadlines,
without listing the repository. A newly observed head is revisited when its settle window ends;
draft or running-check sources are retried every two minutes. Capacity waits first inspect local
task state and make no forge request until a slot opens. Failed direct reads retry on the same
bounded interval. A full repository listing remains necessary to discover new, closed or reopened
sources and heads that change while no follow-up is pending. Validated source-action CI retries
use a separate two-minute cadence rather than the repository scan interval.

Only a global scheduler or administrator scan retires tracking for removed projects; a project-scoped
scan cannot change another project's history. The scanner lists every open PR on explicit
repositories and checks configured target branches. GitHub discovery uses the complete paginated
list to identify new, changed and missing sources before fetching mutable PR details and checks
for candidates. An unchanged, already evaluated head needs only the list; a candidate is fetched
again and validated before task preparation. Other providers may return full details in the list.
The first matching author-and-title handler owns eligibility; a waiting match cannot fall through.
Bot-authored PRs and
`artidezi/` source branches are excluded. Watched forge/repository/target ownership may appear in only
one project. A previously open tracked source missing from the listing is fetched individually;
absence is not proof of closure. Confirmed closed sources with unfinished linked tasks are still
checked until those tasks settle. Closed sources with no active task need no individual fetch:
reopening makes them appear in the open listing. List truncation is an error rather than a silently
complete scan.

Tracking lives in [the source and schedule tables](../../crates/store/src/migrations/0002_pr_discovery.sql),
separate from tasks. It retains project/repository/forge identity, PR number/URL, handler, observed head,
head observation time, follow-up deadline/reason, last task, run count, pause flag and status.
Older records without a follow-up field remain readable and gain one on their next full scan.
Repository, author and head
identity are not inferred from untrusted text. A configured title regex is only an additional
narrowing predicate after explicit repository and author scope.
The Incoming changes UI links each tracked source to its latest task. A completed task marks the
source as evaluated when the task's frozen source identity and head match the latest observed head;
the task can finish between scans without leaving the row labeled as waiting. Source closure and
the Open/Paused/Closed tabs continue to describe the request itself.

## Eligibility and reactivation

| Condition | Behavior |
| --- | --- |
| No discovery, unmatched or ignored author | No task |
| Draft, running checks or unsettled head | Wait without a model call; revisit the known source when due |
| No reported checks after settling | Queue an evaluation; source approval/merge still requires passing checks |
| All reported checks terminal, including failures | Queue an evaluation; failed CI is evidence for the assessment |
| Existing active evaluation | Do not overlap it |
| Queued evaluation has an old head | Cancel it; consider the newest head on a later scan |
| Source closed/merged | Cancel queued or leased work, even if evaluations are paused; do not activate again while closed |
| Completed assessment of same head | Keep it; metadata changes do not re-run |
| Previously closed source reopens with a new head | Renew budget and clear pause/latest-task link only after the prior task and attempt are terminal and all adaptations are resolved |
| New eligible source head | Queue within project capacity and the persisted source budget |
| Failed/lost/needs-human/interrupted work | Hold for inspection, apart from the bounded lost and transient retries a pool opts into |
| Valid approval/proposal superseded before any publication activity | Close the obsolete task; settle and evaluate the new head within the existing budget |
| Exhausted run budget or explicit pause | Hold until explicit human action |

Capacity counts working source-PR tasks per project; it is not a global instance quota or the worker
count. Run budgets count queued evaluations, including superseded ones. Explicit resume renews the
budget and permits a same-head evaluation. Pause cancels queued work; a leased attempt may finish
while further automatic runs stay held. Configuration routing/identity changes require deliberate
handling instead of silently reassigning existing work.

Reopening is detected from the persisted closed flag before replacing the observed state/head.
Resolved-proposal checks inspect durable publication-plan branches on the forge, including proposals
whose creation receipts were lost. A fresh lifecycle retains immutable task/source history and still
applies normal eligibility gates. Same-head reopenings and head changes on continuously open sources
retain their existing budgets and holds. A close/reopen cycle missed between scans requires explicit
resume to renew a held source.

Source closure requires a confirmed forge response; a missing list entry or failed read cannot
cancel a running evaluation. Detection follows the configured discovery poll interval. Cancellation
revokes the lease immediately; the worker observes it on its next heartbeat and stops execution.
Completed results and published adaptations retain their existing reconciliation/inspection rules.

## Frozen input and result handling

The Forgejo adapter supplies the PR-head ref from the configured target remote. The controller fetches
it into its cache and verifies that its SHA equals the API's observed head. A changing source during
snapshot preparation is surfaced and held; the agent never chooses a clone URL or publishes to a
source branch. All target/context bases and source input are captured in immutable bundles/policy.

The source can be behind its target. The prompt asks the agent to identify the PR's own changes
from their merge base, then evaluate integration with the recorded target. A two-tip tree diff can
show newer target work as absent from the source; that absence is not a change made by the PR.
Replacement candidates must preserve unrelated target work. This is agent guidance, not an
independent semantic guarantee supplied by the controller.

When the matched handler requests CI results, the GitHub adapter's already-fetched complete visible
commit-status and latest check-run listing is normalized and frozen in the source policy alongside
the exact head. Prompt template 9 renders the overall state and each name/status/conclusion as an
explicitly untrusted point-in-time JSON block. When separately permitted, the controller uses a
single-repository Actions-read token to find completed failed jobs at that head, verifies their run
attempt and exact job identity, and freezes up to two sanitized, bounded excerpts as private content-addressed
artifacts. A failed or unavailable log fetch records an outcome without suppressing task creation.
The worker receives only the selected excerpt in its task document, not the token or signed URL.
The bounded excerpt preserves a strong diagnostic from the final 64 KiB of sanitized output when
cleanup or teardown would otherwise displace it from the final lines.
Python diagnostics retain bounded endings of the last two tracebacks, including their exception
messages, rather than only their opening frames. Sections that overlap are merged and omitted
byte counts remain explicit within the existing excerpt limit.
Historical policies omit these optional fields, and revisions take a fresh snapshot from the newly
inspected source.

The task, initial attempt and updated tracking claim are inserted in one SQLite transaction. Input
artifacts are prepared first. The task uses `pull_request` kind. Default dry-run handlers force `shadow` delivery and clear all
effects. Explicit publish handlers force `merge_request` delivery and freeze the publishing bot. Both
clear tagging authority. Source merge permission is retained only for explicit merge handlers;
the source snapshot also freezes `safe_verdict_action`, defaulting historical tasks to comment-only.
Title-scoped tasks freeze the matched title. The handler-selected or default agent profile is resolved
through the project allowlist and frozen with the task policy. Repository snapshots include
provider/API/bot identity.
The worker job can therefore use the existing version-1 input-ref contract.

In dry-run mode, before recording an accepted result, the controller rechecks source head, state, target, author and
draft status. Title-scoped tasks also require the exact matched title to remain unchanged.
Changed/closed input becomes obsolete history; a later scan can evaluate the latest
eligible head within the same budget. A current `approve_untouched` or `proposal` completes the
assessment without publication. The normal publisher also rejects shadow delivery defensively.

Authentication and CSRF protections apply to UI scan/pause/resume and feedback; admin routes require
the separate admin token. Feedback on the current source task creates a new linked assessment using
current configuration and inputs. Feedback on an older task cannot overwrite a newer evaluation.
The [Incoming changes view](../../crates/server/src/ui/incoming.rs) aggregates stored tracking across
projects. Its GET and polling requests never scan; POST scan chooses a project or all configured
projects, while pause/resume target one source. Structured project/search/status/page fields preserve
the view after an action without accepting a redirect URL. The UI calls `shadow` delivery **Dry run**;
stored contracts and scheduler status comparisons retain their existing values.
The [discovery regression suite](../../crates/orchestrator/tests/support/discovery.rs) exercises these
transitions; the real Forgejo suite verifies its PR-ref behavior.

Publish mode uses [source publication](../../crates/orchestrator/src/source_publish.rs): current
source, target/context and authority checks, durable plans, zero-lease branch creation, owned PR
receipts and COMMENT review deduplication. Incomplete publication is held for explicit retry.
Source-head mismatches use a structured error with the frozen and observed commits. An otherwise
eligible valid approval/proposal can be retired automatically only before publication plans/effects
or `assessment.*` events and with no proposal receipts. `publication.retry_requested` alone is not
a remote effect and does not block recovery, including eligible stale holds left by an older release.
A recognized transient execution failure with no result can likewise retire when discovery already
observed a newer head, without retrying its obsolete inputs or overriding explicit source pauses. The persisted follow-up waits for the new head
to settle; its run count is retained, and exhaustion pauses it. Other authority/identity changes,
failed/needs-human results and explicit operator pauses retain their holds. CI-wait
reconciliation uses the same guard; discovery leaves that decision to the delivery reconciler.
Awaiting proposals suppress new evaluations; source movement holds them. Resume/feedback checks
planned branches for open PRs before creating another task. Human merges are observed without
source-merge or tag effects. See [GitHub internals](github-integration.md#scoped-publication).

Source approval/merge uses the additional policy and recovery gates described in GitHub internals.
Issue intake and comment commands remain future work.
