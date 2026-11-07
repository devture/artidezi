<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Security and storage

## Trust boundaries

The controller owns forge/admin credentials, repository scope, resolved policy and effects. Treat
PR text, source code, release notes, model output and worker uploads as untrusted input. A convincing
instruction in a source repository cannot grant additional repositories, publication modes or tokens.

The supervisor mechanically records work, but runs in the same execution environment as the agent.
The current full-access Codex path trusts the provisioned worker VM as its execution boundary. The
harness runs as the supervisor's user, so an agent can read the worker token and model credentials
without any Docker access. With the worker token it can lease other queued jobs in the same pool.
Fresh attempt directories are useful hygiene, not a reset of a compromised machine. Separate
trust domains need separate worker environments and credentials.

Proposals are pushed as `artidezi/<task>` branches in the target repository, so the forge's CI may run
them before anyone reviews them. Securing that CI is the operator's job: see
[Prepare the repository's CI](../user-guide/projects.md#prepare-the-repositorys-ci).

`read_only`, allowed paths and expected commits are output checks; they do not implement operating
system isolation between checkouts. Network declarations in a job are not an external firewall.
Provisioning must enforce the intended command egress/private-network policy outside an expendable
worker when that isolation is required. The application does not create a VM/container sandbox per job.
The shipped local scripted stack has a different, constrained topology described in the development
guide; it is not Docker-in-Docker or a complete VM simulation.

## Credentials and transport

| Credential | Location / authority |
| --- | --- |
| Forge credential | Controller only; API/Git access to configured repositories |
| Admin bearer token | Controller/admin client; task and discovery administration |
| Worker bearer token | Controller/one worker; leasing from its pool and operations on its own attempts |
| Model credential | Selected worker credential root/harness home; no forge publication authority |
| UI user hashes/session | Named browser access, separate from API/worker tokens |

The [worker routes](../../crates/server/src/worker.rs) check that the calling worker holds the active
lease before attempt operations. Each token belongs to one listed worker, so removing a worker from the
configuration revokes it, and one worker cannot act as another. A worker cannot use its token as an admin token or read arbitrary
instance artifacts. The [UI](../../crates/server/src/ui/mod.rs) uses expiring opaque sessions,
CSRF/Origin protection and escaped content. Browser sessions and named accounts live in SQLite, with a
30-day absolute session expiry.
Only a SHA-256 digest of each random bearer is stored with its CSRF token, named-login scope and
expiry. Expiry is checked on
browser requests, and logout deletes the stored session. The 128-session cap excludes expired
entries and allows a browser to replace its own login. Restarting preserves unexpired sessions.
Accounts and grants are resolved from SQLite on every request; disable and grant changes affect
the next request. Password updates revoke every session for that principal. Optional `ui_access`
configuration reconciles names, enabled state, administrator status and grants at UI startup,
while preserving current password hashes. Omitted accounts are disabled and sessions revoked.
Initial password hashes in config require private handling and are excluded from ordinary config
serialization and debug output. Named browser actor strings include a stable ID and display
snapshot; admin API actor strings remain caller-supplied labels. Users can change their own
passwords in the browser; a local CLI can inspect accounts and reset passwords for recovery.
New browser passwords use Argon2id with a random 16-byte salt, 19 MiB of memory, two passes,
one lane and a 32-byte output. The versioned PHC string records those parameters; config and
stored-account validation reject other schemes or costs before verification allocates memory.
TLS termination and host/network policy belong to deployment. See [operations](../user-guide/operations.md).

Do not place secrets in project descriptions, recipes, fixtures, bundles or task prompts. Even without
intentional credentials, source and logs can contain sensitive data; treat artifacts as private
instance state. Do not copy real model credentials into default local-demo configuration.

Each Codex worker using `chatgpt_login` needs a writable durable credential directory and its own
independently enrolled login. Copying one rotating `auth.json` to multiple workers makes them share a
single-use refresh-token lineage: whichever worker refreshes first can strand the others. Attempt-local
copies are validated and persisted back only to that worker's unchanged source file; ArtiDezi does not
synchronize model credentials across machines.

## Verification limits

The controller validates uploaded Git objects, metadata and declared checks against a frozen job;
it does not execute repository code. Supervisor checks provide evidence beyond the agent's prose,
but do not prove independence from the worker. Hashes establish content identity, not trustworthiness.
A successful test also establishes only the behavior its assertions cover.

Opted-in source CI summaries and selected failed-job log excerpts are forge-supplied input, not
controller verification. Check names, states and logs are rendered as untrusted data and may be
stale by the time an agent responds. Log text may contain secrets despite GitHub masking and control
normalization. The project permit means selected excerpts enter private artifacts, task documents,
backups and the model worker; there is no automatic retention expiry. The controller still refreshes
its independent publication gate before any source action.

A future independent verifier would need a clean separately provisioned execution boundary, exact
candidate trees, trusted test selection, constrained network/resources and no forge/model credentials,
with reset between jobs. It is not implemented. Source auto-merge is enabled only by explicit project
policy and an accepted `approve_untouched` result, while current forge checks, fresh source state and
review/authority gates remain enforced. Worker risk ratings and evidence are recorded context rather
than an independent controller veto. Repository rules govern merge protection and read/effect races;
the operator must accept this trust model. Dry-run results have no merge effect.

## Durable state

[The store](../../crates/store/src/db.rs) uses SQLite/WAL with explicit state transitions and migrations.
Tables hold tasks/policy snapshots, repository bases, attempts, leases, published PR references, audit
events, tracked source PRs and scan deadlines. Data stays on a local durable filesystem. The process
assumes one orchestrator per data directory. Cloneable database handles share a single connection;
each method holds a short connection lock across its queries/transaction. Task/forge operations stay
serialized separately, while UI reads and worker transport remain available during repository I/O.
HA/multiple orchestrator processes are not an implemented deployment mode.

Artifact transfers run outside the database lock. Concurrent writes use distinct temporary files
and atomic renames, including when multiple requests upload identical bytes. Download authorization
checks the active attempt and its artifact references; revocation may race an already authorized
transfer. Final record acceptance rechecks the lease/state through the serialized controller, so
uploaded bytes alone cannot authorize publication.

Task numbers live in a separate `task_numbers` table and are assigned by an insertion trigger.
Migration 3 backfills them by creation time, then internal ID to break ties. SQLite AUTOINCREMENT
keeps later allocations monotonic; the original task ID remains the foreign-key/job/forge identity.
Browser numeric routes resolve the number before using the existing task operations. Numbering is
instance-wide and persists through restart; it is not an authorization mechanism.

[Artifacts](../../crates/store/src/artifacts.rs) are stored by SHA-256 and size under
`artifacts/<first-two-hex>/<sha256>`. Normal byte reads verify content identity; streaming path consumers
must perform the corresponding validation. The store contains job/prompt/result/attempt documents,
Git bundles, patches and logs. Hash-addressed content is immutable; database references connect it
to work history. Artifact staging precedes some database transactions, so interrupted preparation can
leave unreferenced files. Opt-in [artifact retention](../user-guide/artifact-retention.md) removes
completed tasks' frozen repository inputs and old unreferenced files. Results, patches, logs,
worker-produced commit bundles and immutable job/attempt documents remain reachable evidence.

Cleanup runs under the controller operation lock and a store lifecycle barrier shared with worker
uploads. An authorized upload records its attempt/hash/size pin before storing bytes, including
deduplicated uploads whose existing file mtime is old. Active/recent tasks and open published
proposals protect their entire input graph; all references from stored policies, source tracking,
audit payloads and retained documents protect shared content. Cleanup skips only the job contract's
snapshot/input bundle fields and the frozen policy's duplicate source input bundle for eligible
terminal tasks. Source identity and CI evidence stay retained. It does not infer expiry from filename,
worker media type or a file's apparent Git contents. Output bundles can contain unpublished work
and do not expire. Old unreferenced completed objects have a separate grace period; staging files
under `artifacts/tmp` and Git caches are outside this collector.

Before marking, the controller refreshes recorded open proposals of retired tasks using read-only
forge queries bound to their frozen repository/forge identity. Observed closure or merge updates
the publication record and audit history, without task transitions, remote writes, tags or delivery.
Read failures leave the publication pin intact and are returned as cleanup report diagnostics.

The mark phase verifies retained documents and preflights protected files before any deletion.
Malformed required job/attempt documents, missing protected references and inconsistent sizes abort
cleanup. Model-result bytes are retained as opaque evidence, including malformed JSON from failed
attempts; their content does not define artifact reachability. Migration
11 stores upload pins, irreversible per-task input expiry and per-hash deletion receipts. Receipts
use a `synchronous=FULL` commit before unlinking, temporarily overriding the normal WAL mode
under the connection lock. This also flushes preceding task-expiry records. Interrupted sweeps are
retryable and intentionally expired inputs
are distinguishable from unexpected loss. Shared files remain until every live reference releases
them. Job bytes and schema versions remain unchanged; expiry ends exact offline replay of those
inputs, rather than rewriting historical jobs. Fresh tasks fetch fresh inputs from the forge.
Changing retention durations or disabling cleanup does not restore previously expired inputs.

Git caches live under `caches/` and contain fetched/imported/recreated objects. Published history can
be refetched, but unpublished candidate material and active work require care; retain the whole data
directory for a straightforward backup. SQLite state and referenced artifacts must remain consistent.
Configuration and credentials are separate backup concerns. At-rest encryption, evidence retention schedules,
clean verifier provisioning and a tested operator restore workflow are future operational decisions.
