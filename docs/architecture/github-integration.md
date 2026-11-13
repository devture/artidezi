<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev
SPDX-License-Identifier: Apache-2.0
-->

# GitHub integration

The [enrollment guide](../user-guide/github-enrollment.md) covers operator setup. GitHub.com intake
uses the existing project/discovery model: ordered author rules with optional title narrowing,
polling, source-head deduplication,
settling, bounded evaluation budgets and explicit holds. No webhook server, issue intake, required
catch-all or general event engine is introduced.

## Authentication and reads

[`GithubForge`](../../crates/forge/src/github/app.rs) implements reads and explicitly scoped publication in `ForgeAdapter`.
One binding has an App client ID, installation ID/account, bot login and PEM secret reference.
Configured project repositories form its allowlist. The controller signs RS256 JWTs using OpenSSL,
checks App/client/bot and installation identity, requires the requested permissions, and issues a token for
one configured repository at a time. Both selected-repository and all-repository installations are
accepted; neither automatically enrolls projects. Returned scope and expiry must match expectations.
Tokens are cached with a two-minute renewal margin; renewed tokens recheck installation identity.
A rejected repository token clears cached credentials so a later request can authenticate again.
Keys, JWTs and tokens never enter policy snapshots, worker jobs or result records.

The fixed-origin [HTTP layer](../../crates/forge/src/github/http.rs) permits GETs, installation-token
issuance and typed publication POST/PATCH/PUT operations. Mutations are never automatically retried. It refuses redirects, bounds responses to 8 MiB, suppresses response/error
body contents and supports bounded ETag caching. Expired or failed requests cannot return cached
success. Server retry/reset deadlines are honored in memory and persisted in SQLite
by discovery/result processing; forced scans still respect that backoff after restart. Poll timing
is persisted before contacting the forge. Repository-wide scan errors wait for the configured
poll interval; failed direct follow-up reads retry on the bounded follow-up cadence.
Preparation/result failures retain their explicit hold behavior.

The adapter resolves target branches, lists sources and reads exact-head statuses/check runs.
`refs/pull/N/head` is fetched from the configured target repository, including fork contributions;
API-provided clone URLs and fork destinations are never followed. The controller verifies the
fetched PR head and target branch against their reported commits before building bundles.
Credential callbacks accept only the original Git URL; Git HTTP redirects are disabled.

The visible CI summary is conservative but is not required-check/branch-protection verification.
An empty check set is distinct from pending; all-skipped/neutral checks are terminal but cannot
authorize automatic source action. Running or unknown checks keep the summary pending even when
another check has already failed. The read-only
inspector uses the same checks parser. API file patches are optional/incomplete and never substitute
for the Git bundles used by the worker.
For an opted-in incoming-change handler, the adapter also retains the complete visible status/check-
run names and states from that same read and freezes them into agent context. A separate project
permit and handler opt-in can also collect bounded failed Actions job excerpts with an Actions-read
token restricted to one configured repository. The adapter checks the source SHA, run attempt and job
identity through Actions APIs; it never follows check-run details URLs. It sends the bearer only to
the GitHub API, then downloads the short-lived signed log redirect without that credential. A missing
or rejected optional log keeps an explicit unavailable outcome. All CI context is untrusted,
point-in-time evidence and never weakens the fresh publication gate.

## Scoped publication

Each handler defaults to `mode: dry_run`. An optional title regex narrows an explicit author rule;
it never replaces repository or author authority. A `publish` handler also requires the project publication
effect; tasks freeze this choice. Dry runs clear all effects and tagging. Publish tasks permit
COMMENT reviews and human-reviewed adaptation PRs by default, with an explicit source-action
policy for approval/merge. Tagging is always cleared; the merge effect is retained only for merge handlers. Historical dry-run tasks cannot be promoted.

The GitHub binding derives its writable repository set only from explicitly enabled publication
projects, excluding read-only context. Read tokens always request exactly five read permissions;
publication uses a separate token cache with Contents and Pull requests write. Returned token scope
must exactly match the request even if installation permissions are broader. Separate repository sets
grant approval and merge capabilities, derived only from opted-in source handlers. One-off manual tasks
are allowed for configured projects, while publication, tags and branch deletion remain controlled by
explicit project effects. The Git push boundary asks for scoped
publication credentials; worker documents contain none of them.
Superseded and merged proposal branch cleanup uses those same single-repository publication
credentials and Git's exact-head lease on both GitHub and Forgejo. The GitHub publication token already requires
Contents write; cleanup adds no App permission and does not use an unconditional REST ref deletion.

An explicitly authorized source proposal that changes `.github/workflows` uses a third token cache.
That token adds Workflows write for one configured repository only; ordinary publication tokens never
include it. The App installation must independently grant Workflows write. No project or handler
setting grants workflow publication to future tasks.

[`source_publish.rs`](../../crates/orchestrator/src/source_publish.rs) verifies frozen and current
repository identity (provider/API URL/bot/remote/branch), allowed paths, access, handler ownership,
source eligibility/head and target/context bases before effects. Workflow modifications in any
proposed GitHub commit withhold the whole publication before its first push unless an authenticated
admin recorded `publication.workflow_authorized` for the exact validated attempt-record digest.
The grant event records the actor, reason, source head and affected repositories before retry. A
new attempt cannot inherit it. Retry still uses the original validation, fresh scope/freshness gates,
durable plan and proposal receipts; a failed or lost response never authorizes a second candidate.
These checks bound
authority; they do not prove semantic correctness of agent reasoning or patches.

A `publication.planned` event stores the recreated commit, local ref, branch, title and body before
Git push. Initial push requires an absent branch through a zero-OID lease; recovery accepts only the
planned head. All-state proposal lookup distinguishes a lost response from absent, closed, edited
or foreign publication. Owned review markers plus exact body and source SHA recover COMMENT reviews
without duplication. This is reconciliation, not an atomic transaction with the forge. External
changes can still race the last check and effect; later checks hold the task, without deleting effects.

Errors pause the source. A restart finds working publication tasks whose results are already valid
and holds them for explicit retry. Retry loads the original validated artifacts and durable plans;
it never reruns the model. Current scope/freshness gates still apply. Pending plans depend on their
recreated Git objects: retain caches alongside durable state until publication resolves. Missing
objects fail safely. Fresh evaluation/resume checks all planned branches for open proposals,
including lost receipts. It cannot create replacement tasks while those proposals remain open.

After delivery, reconciliation observes human merges of the recorded proposal heads and attempts
leased cleanup of their branches. Cleanup is independent of delivery success and can retry after
the task becomes done; see [proposal cleanup](lifecycle.md#proposal-branch-cleanup). A moved source
can also close owned obsolete proposals and attempt leased cleanup of their recorded branches;
[supersession](pr-discovery.md) describes the ownership, recovery and nonblocking failure rules.
Changed proposals require attention. Source approval/merge is a separate path; there is no automatic source
closure for replacement proposals or direct release operation.

## Verification

The test-only loopback fixture uses disposable RSA keys and verifies JWT signatures/claims. It
checks installation identity, scope, renewal/revocation, conditional requests, permission errors,
backoff and refused redirects. Fixture support is compiled only for tests or the explicit
`test-fixtures` feature, never enabled by YAML or ordinary release builds.

The full controller test combines that App HTTP adapter with disposable local Git transport and
the scripted worker: source discovery, frozen fork input, context repository, candidate patch,
verification, local result recording, restart deduplication and pause/resume. It asserts zero
repository writes and that credentials stay out of worker documents. Separate cases cover pending
CI, moved source heads, persisted backoff and old queued publication after a forge becomes read-only.
Publication cases exercise one COMMENT, adaptation delivery, lost PR/review responses, restart
recovery, forbidden workflows, moved context and authority changes. Multi-repository tests observe
human merges without release effects. Forgejo tests exercise real Git authentication, COMMENT,
change-request and APPROVED review receipts, and source approval and merge of another user's PR.

Source review bodies keep concise verdict/risk/question context visible and place the complete
agent evidence list in a collapsed disclosure. Evidence-note formatting preserves inline code,
identifiers and HTTP(S) Markdown links, escapes raw HTML and neutralizes mentions. This presentation
does not promote agent prose into verified controller evidence.

Local fixtures prove these contracts, not a particular live App's registration or worker tooling.
Run `github-check`, verify the real installation and review pilot outcomes before widening access.

## Safe source actions

The source snapshot freezes `safe_verdict_action` (`comment` by default, omitted on serialization
for historical snapshot-digest stability). Title-scoped tasks also freeze the selected title and
require it to remain exact through delivery. Approval/merge rechecks that current policy still grants
that action. Accepted unchanged results qualify when the configured source checks and review gates
pass. The controller requires fresh green forge checks regardless of intake gating, and rejects
outstanding change-request reviews. Worker risk ratings, evidence and questions are retained as
audit context; they are not a second independent veto over the configured action.

An `assessment.approval_planned` event precedes APPROVE. Retry accepts exactly one matching bot,
commit, body and APPROVED state; edited, dismissed or missing approvals are not recreated.
If a planned approval has no matching receipt, explicit retry stops; inspect before a new evaluation. A
`source.merge_planned` event precedes the merge API call, which includes the expected head SHA.
A successful response records `source.merge_accepted`. A matching merged source plus the exact owned
approval records `source.merge_observed` and completes without another write. This also handles
another actor merging immediately after approval and lost approval/merge responses. Observing the
merge does not assert the actor when the receipt was lost. Paused/superseded tasks cannot recover
or publish without the existing explicit retry path.

The merge endpoint has no expected-base parameter; APPROVE has no atomic expected-head condition.
Read/effect races remain, so repository required checks, up-to-date rules and stale-review dismissal
must enforce the operator's final policy. Do not grant the App bypass permission. Missing CI stays
pending and errors never become green checks. ArtiDezi neither changes protection nor falls back to
another merge method. Failed gates hold the task for inspection/retry. Pending checks on an otherwise
eligible unchanged result use a distinct `waiting_for_ci` state. Failed checks hold the task too,
but that hold is rechecked (below).

[CI continuation](../../crates/orchestrator/src/source_ci.rs) stores the waiting transition and a
next-check deadline atomically in SQLite (`source_ci_waits`). Maintenance reserves the next deadline
before forge reads, on a fixed 2-minute cadence. Repeated pending checks
do not add duplicate activity events. Waiting occupies no worker or active-evaluation slot; discovery
does not create another task for that source. Removed projects/rules are still checked and held.
Preflight repeats authority, source/context freshness and review checks. Forge backoff keeps the
task waiting; other read failures require attention. A known successful CI preflight changes the
task to `working` before effectful delivery, which repeats the existing gates and reconciles exact
receipts. If checks become pending again, delivery can return to waiting. Ambiguous write failures
remain explicit recovery, including crashes after entering `working`. Human pause/close, fresh
evaluation or any other departure from waiting deletes the scheduled check. Human holds
are never promoted automatically, with one exception.

The source-action gate reports failed checks as a distinct error. During first delivery that gate
runs before an approval is planned and again after it is published. An explicit retry can also
reach it after a lost approval or merge response, so a planned approval without a published receipt,
or any planned merge, keeps an ordinary hold. Otherwise the controller pauses the source and, in one
transaction, records a `task.needs_human` event marked `"recheck": "failed_checks"`, enters
`needs_human` and schedules a `source_ci_waits` deadline for the held task. Maintenance claims that deadline on the same cadence
only while the marked event is the task's latest hold, no `discovery.paused` event follows it and
the source still links and pauses for this task. If the frozen head's checks are green, it records
`delivery.checks_recovered` and runs the ordinary publication retry, which repeats every gate and
reuses an exact existing approval. If the head moved and no reviewer's latest decision requests
changes, it closes the task through the superseded-result path (`assessment.superseded` with `"cause": "failed_checks"`) so discovery assesses the new head
after settling within the retained budget. Red, pending or absent checks and closed sources keep
the hold; forge read errors retry later; other errors convert it into an ordinary hold.

Local HTTP/worker tests cover explicit approval versus merge, controller safety gates and advisory
risk/evidence context, CI pending/failing, review objections, moved source/target, authority revocation, API head races,
protection rejection, lost receipts, external automerge and adaptation PRs retaining human review.
CI-wait tests cover due-time persistence, no duplicate evaluations/effects, approval followed by new
pending checks, cancellation, policy revocation and unchanged worker artifacts across restarts.
Independent verification and direct release automation remain future work.

