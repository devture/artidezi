<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Task and attempt lifecycle

A task stores the project's resolved policy/digest, instructions, selected profile/delivery mode and
repository bases. An attempt is one immutable job and execution record within that work history.
A tracked source PR can link several tasks across changed heads and explicit human follow-ups.
These are separate identities because completing an assessment must not erase its original inputs.

## Manual workflow

1. [Task creation](../../crates/orchestrator/src/tasks.rs) validates the selected project/profile/mode,
   freezes policy, resolves target branches and creates repository bundles and a job artifact.
2. A compatible worker leases the queued attempt. Its pool, supported schemas and installed harness
   version constrain dispatch. Heartbeats extend the lease within a fixed execution ceiling.
   Expired execution becomes lost; opted-in pools can queue a bounded replacement before any
   accepted record/publication, otherwise the task needs human attention.
3. The worker prepares inputs, runs setup/harness/verification, then uploads artifacts and its record.
4. [Ingestion](../../crates/orchestrator/src/ingest.rs) validates identities, hashes, result semantics,
   repository collections, Git series and configured verification records against the stored job.
5. A valid outcome goes through [publication](../../crates/orchestrator/src/publish.rs). Invalid,
   failed or uncertain work goes to human attention with the evidence preserved.
6. [Maintenance](../../crates/orchestrator/src/maintenance.rs) reconciles proposals with the forge and
   completes delivery when the required changes have landed.

| Task state | Meaning |
| --- | --- |
| `working` | Queued/running or processing an attempt |
| `waiting_for_ci` | Accepted unchanged source assessment waits for CI before automatic approval/merge |
| `awaiting_review` | Published proposal(s) await review/merge |
| `needs_human` | A question, invalid/failed result or uncertain delivery requires attention |
| `done` | Delivery completed, or a shadow assessment was recorded; terminal |
| `closed` | Stopped or superseded; terminal |

Attempt states separately track queue, lease, upload and validation, including cancellation/loss.
The legal transitions are implemented in [store models](../../crates/store/src/model.rs) and enforced
by [database operations](../../crates/store/src/db.rs). Task success and attempt validity are different:
a valid candidate can still encounter a publication conflict or remain purely advisory in shadow mode.

## Revisions and publication

Manual revision creates a new attempt under the original frozen policy, resolving fresh target bases
and supplying the original task, the latest feedback and the last attempt's available record, result
and series. The agent starts a new session and explicitly re-applies useful changes from that context;
it does not resume the prior conversation or checkout, and does not receive every earlier attempt.
A lost/unreported attempt has no previous artifacts: job version 2 allows their absence while still
carrying feedback in `task.md`. It retains the task and existing proposal identity. Terminal manual
tasks cannot be reopened through revision.

Accepted commits are recreated with the controller's bot identity in its bare Git cache. Before
updating a proposal, the controller checks forge ownership and expected branch head. Changed foreign
branches, already merged proposals and moved direct-push targets are not blindly overwritten.
Ordinary delivery creates one PR per changed writable repository; direct fast-forward delivery is
restricted to a single writable target. Merge/tag behavior additionally depends on configured policy
and current forge state. Tag proposals from the agent are advisory and constrained by tagging policy.

All repository series/checks are validated before starting publication. Cross-repository forge writes
are nevertheless not atomic. A failure after one PR is created preserves that progress and moves the
task to human attention. Do not assume retrying an arbitrary failed HTTP call is safe. Reconciliation
and ownership checks matter before any retry of a forge side effect.

## Proposal branch cleanup

Every recorded proposal observed as merged queues cleanup in the same SQLite transaction as its
merged receipt. This covers human merges, manual-task automerge and the retired-proposal refresh
used by artifact retention. Cleanup runs inline when possible, for each repository separately;
unmerged proposals retain their branches. Cleanup cannot prevent `done` or change tag/delivery
success. Source approval/merge acts on an incoming PR rather than an owned published proposal and
does not request branch deletion.

The shared [cleanup helper](../../crates/orchestrator/src/proposal_branch_cleanup.rs) also serves
supersession closes. It rechecks frozen repository identity, current publication/write authority,
bot ownership, source branch, target, recorded head and the expected terminal proposal state.
Open proposals and the owning task's unreceipted publication plans protect the branch. A durable
`merge_request.branch_delete_planned` precedes deletion through publication Git credentials and an
exact-head lease. An absent branch succeeds, including forge automatic branch deletion on merge.
A moved branch is refused without adopting its observed head.

Merged cleanup has at most three attempts, one minute apart. The queue stores an immutable receipt;
each attempt reserves its next deadline and consumes its budget before forge/Git I/O, so restarts
cannot reset the lease, budget or cadence. Maintenance selects at most 16 due rows through an index,
even for terminal tasks, without scanning task event histories. As with other forge maintenance,
instance discovery pause suspends processing. Temporary read/push failures record
`merge_request.branch_delete_retry_scheduled`; authority/ownership refusals, stale leases and
exhausted attempts finish with a skipped or failed outcome. A crash after the third reserved try
can leave one final bookkeeping pass, which records exhaustion without another forge/Git attempt.

An unfinished deletion plan retries only its original lease, including when deletion succeeded but
its response was lost. Terminal deleted/skipped/failed events precede queue completion; a crash
between them completes bookkeeping without touching a recreated branch. Completed queue rows
prevent repeated merge observations from enqueuing cleanup again. Supersession closes retain their
existing inline crash recovery and do not gain failure retries. No migration backfills historical
merges, and human-closed proposals and incoming source branches are not swept.

## Source-PR workflow

Discovered tasks freeze an additional source head. Dry-run handlers record the assessment without
forge writes; publish handlers can deliver reviews or replacement proposals within their configured
authority. Source freshness is rechecked before treating the assessment as current.

A new eligible source head creates a new linked task. Explicit source feedback also creates a new task
using current configuration/inputs, closing an older nonterminal assessment where applicable. The
previous completed task remains terminal. Source holds/budgets live outside the task and therefore
survive this new task creation. See [discovery internals](pr-discovery.md).

A validated unpublished approval/proposal whose source head is superseded can be closed
automatically, with an `assessment.superseded` event retaining the old and observed heads. Discovery
assesses the new revision under its ordinary gates and the remaining source budget. Any publication
plan or receipt, identity change, human decision or failure prevents this recovery. A retry request
that stopped before any publication plan does not prevent a fresh evaluation. Published
adaptations whose source moved while it stayed open are retained when Git proves that the source
still carries exactly the frozen change, measured from each head's merge base with its own target.
The task stays awaiting review, without comments, branch updates, a new assessment or another budget
charge. A durable `source.equivalent_head_observed` event binds the task, both source heads and both
target commits; it is not an assessment or publication receipt for the new head. Merging the retained
adaptations completes delivery normally, and discovery remembers that delivery for the observed head.

Without that proof, published adaptations are closed on the forge first, when they are provably the
task's own and unchanged, and the task retires as before. Fetch errors, missing objects and ambiguous
or empty changes do not establish equivalence. Older stale-head holds are reconsidered automatically,
while later explicit operator pauses remain in force. Unpublished results, including CI waits and
failed-check recovery, still supersede even across an equivalent rebase: no adaptation has yet been
delivered to preserve. Source reviews, approvals, merges and recovery attribution always require the
exact frozen head. Revising an existing adaptation also refuses a moved source; retaining a proposal
does not authorize publishing new work against that head.

## Recovery constraints

Lease expiry and lost-execution recovery run every five seconds through local state access,
independently of the serialized forge timer. The default lease is 120 seconds; a healthy available
worker can lease a replacement after expiry and its next poll. This timing assumes a responsive
controller, database and local storage. An alive supervisor can still heartbeat while a command
is stuck; command timeouts and the independent frozen execution ceiling bound that case.

A worker retries its downloads, uploads and report for up to one lease TTL while the orchestrator
is unreachable, so a restart doesn't discard the attempt.

`worker_pools.<name>.lost_attempt_retries` opts into zero to three replacement executions (default
zero). A replacement gets a new attempt/job identity and number but retains the exact prompt,
repository snapshots, source head, policy and existing revision context. The prompt's header can
therefore retain the original execution number; the job/attempt record owns the new identity.
It does not consume another source-discovery run or silently refresh instructions. Stored version
1 inputs become a version 2 replacement job without changing their prompt version.

Loss, worker cooldown, replacement linkage and audit events are durable. Replacement insertion
and the eligibility/limit check share one SQLite transaction. Recovery resumes across a restart
between expiry and replacement creation. Only the latest lost attempt of a working task is eligible;
any accepted result, unclassified execution record, published proposal or publication event blocks
automatic rerunning for that task. Classified transient failures can precede a lost attempt. Exhaustion/ineligibility moves it to human attention. Closed tasks stay closed.
Late uploads/records remain fenced by the old attempt's expired lease and cannot publish.

`worker_pools.<name>.transient_failure_retries` independently allows zero to five retries (default
zero) for terminal Codex model-capacity, temporary rate-limit and transport failures with no result
document. Backoff is 60, 120, 300, 600 and 1800 seconds. The controller matches fixed categories from
the final terminal event in a bounded log tail, never command output or model prose. Authentication,
quota exhaustion, invalid results and other validation failures are held for inspection. Each retry
uses a fresh workspace and the same frozen inputs; dirty changes from the failed run are discarded.
Retry limits count all replacements of that category within the task, including mixed loss/failure
chains. The retry deadline, classification and replacement linkage survive controller restarts.
If discovery already observed a newer source head, the incomplete obsolete run is retired instead
and a fresh evaluation is scheduled under the remaining source budget; stale inputs are not rerun.
An error inspecting one held task is reported for that task without stopping discovery/recovery
for unrelated tasks.

A worker that loses a lease receives a persistent five-minute scheduling cooldown, giving other
workers first opportunity. Fresh presence reports do not clear cooldown or operator drain. After
cooldown a recovered worker can serve work again; quarantine still requires environment recovery.
One worker ID can hold at most one live lease. Draining prevents new assignments and allows existing
heartbeats/results to finish.

Expiry is not proof that a process stopped: a partitioned VM, detached service or external Docker
container can remain active. Enable retries only where setup, checks and model execution tolerate
repetition and overlap. Controller fencing protects accepted results/publication, not arbitrary
external side effects from repository code. Accepted-result/publication recovery remains separate
and never starts another model execution automatically.

The server serializes task administration, preparation, ingestion and forge operations through an
operation mutex. UI reads/session operations and worker leases, heartbeats and artifact transfers use
[a separate state handle](../../crates/orchestrator/src/state.rs); they do not wait for that mutex.
All handles share one SQLite connection, locked only for each database operation. No Git, forge or
artifact I/O runs under the database lock. Synchronous work runs on the blocking thread pool;
queued controller operations wait asynchronously before entering that pool, leaving its threads
available for state reads and worker traffic. Once started, an operation keeps its serialization
permit until it finishes even if the HTTP caller disconnects.

Long operations still serialize with one another. Creating/scanning/revising tasks, closing/pausing
work and accepting a final worker record can wait for an in-flight operation. A slow scan request
itself can still time out at a client/proxy while preparation continues; this is not a background
job API. Other pages and worker heartbeats remain available. Configuration is fixed at server startup.
This change adds neither parallel imports/publications nor worker execution concurrency.

SQLite provides transactional state changes, but Git/artifact writes and remote forge operations
are not one global transaction. Source task/attempt/tracking insertion is atomic; staged
artifacts can remain after interrupted preparation. Startup applies migrations and retains audit
history. Interrupted result application, lost leases and partial delivery must surface for inspection.
The implementation assumes one active orchestrator for a data directory; multiple replicas and a
transactional external effect queue are not implemented.

## Incoming publication recovery

Source tasks use the handler's frozen dry-run or publish mode. Publish mode supports COMMENT
reviews, adaptation PRs (human-reviewed unless the handler opts into merging them once checks pass)
and explicitly configured unchanged-source approval/merge.
Durable plans precede Git/API effects. A process restart during incomplete delivery holds it for
explicit retry. A task explicitly in `waiting_for_ci` retains its scheduled check and can resume
the accepted result automatically without another worker. If the head changes before any
publication activity, the superseded-result recovery described above can instead retire it
and let discovery assess the new revision. A hold for failed checks at approval/merge is rechecked:
green checks on the same head retry delivery, and a new head retires the task the same way.
For a proposal that changes a path the forge adapter protects, an admin can record a grant bound to the
validated attempt record and immediately retry. The grant is an event before any forge effect;
ordinary retry can later reconcile a lost response, while a new attempt requires a new grant.
See [source publication internals](github-integration.md#scoped-publication)
and the [operator recovery guide](../user-guide/github-enrollment.md#enable-publication).
