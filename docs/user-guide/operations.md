<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Configuration and operation

See [Installing](installing.md) for a first deployment. Start from the [example](../../etc/example.yaml), the [runnable local configuration](../../etc/services/orchestrator.yaml) and the
[configuration types](../../crates/config/src/model.rs). The
[broader validation fixture](../../crates/config/tests/fixtures/valid.yaml) exercises more fields,
and the [GitHub dry-run example](../../etc/github-dry-run.yaml) shows App enrollment. Replace
placeholder identities and provision the actual worker; these are not ready-to-deploy credentials. See [projects](projects.md) before expanding repository access.

```sh
artidezi validate-config artidezi.yaml
artidezi serve --config artidezi.yaml --secrets-dir /etc/artidezi/secrets --listen 127.0.0.1:8080
artidezi worker --config worker.yaml

export ARTIDEZI_URL=https://artidezi.example.com
export ARTIDEZI_ADMIN_TOKEN_FILE=/path/to/admin-token
artidezi task create --project playground --instructions 'Add a small script and test it.'
```

Secrets are one file per `credential_ref` in the secrets directory. The admin API and each worker
use their own bearer token. Put a TLS reverse proxy in front of the orchestrator; workers
reach it over HTTPS. Keep model credentials on the isolated worker, separate from forge credentials.

## Enabling the web UI

The UI is **disabled by default**. Declare browser accounts and project grants in the instance
config; current password hashes and sessions live in `artidezi.sqlite` under the data directory.
Enable named login with `artidezi serve --ui-enabled`; there is no shared-password login.

### Declarative browser accounts

Generate an initial password hash from a protected password file. Do not put a plaintext
password in shell arguments or YAML:

```sh
artidezi ui-hash-password --password-file /path/to/operator-password
```

Declare every named browser account in the private instance config. The hash is authentication
material and can be attacked offline if copied. Replace the placeholders with command output:

```yaml
ui_access:
  schema_version: 1
  accounts:
    operator:
      display_name: Operator
      initial_password_hash: "$argon2id$v=19$m=19456,t=2,p=1$<salt>$<digest>"
      instance_admin: true
    alice:
      display_name: Alice
      initial_password_hash: "$argon2id$v=19$m=19456,t=2,p=1$<salt>$<digest>"
      grants:
        application: write
        "*": read
```

At least one declared account must be an enabled instance administrator. Validate the config
and then start the service:

```sh
artidezi validate-config artidezi.yaml
artidezi serve --config artidezi.yaml --secrets-dir /etc/artidezi/secrets \
  --ui-enabled --listen 127.0.0.1:8080
```

At UI startup, ArtiDezi applies the declarations in one SQLite transaction. It creates missing
accounts from their initial hashes and updates existing accounts' display names, disabled state,
instance-admin flag and grants. An account omitted from `ui_access` is disabled, its grants are
cleared and its sessions are revoked; its row and historical actor identity remain. Config
changes take effect when the UI service restarts. The transaction rolls back if it would leave
no enabled instance administrator.

**Current passwords are intentionally different:** an existing account's hash is never replaced
by its configured initial hash. Users can change their own passwords on **Account** by entering
the current password and the new one twice. A new account sees an advisory change-password banner
until doing so. A local reset restores that banner. The flag is stored in SQLite and is not
calculated by comparing hashes on every login. Changing the initial hash in config does not
rotate a current password; use a password reset for recovery.

Each account has a personal time zone, set on **Account**. It defaults to the browser's zone at first sign-in, and to UTC until then.

### Permissions and password recovery

IDs are stable audit identifiers and may contain ASCII letters, digits, `.`, `_` and `-`. Grants
name configured project IDs or `*`; `write` includes `read`, and the strongest matching grant
wins. A reader can inspect tasks, incoming changes and evidence in that project. A writer can
also create/revise/close tasks and control that project's discovery. Only an instance
administrator can scan all projects at once; a writer must select a project. Administrators can
view and act in all projects. The UI provides no account creation or role editor: make those
changes in `ui_access` and restart the service.

A local operator with database access can inspect effective accounts or reset a lost password:

```sh
artidezi ui-user list --config artidezi.yaml
artidezi ui-user reset-password --config artidezi.yaml --id alice \
  --password-file /path/to/recovery-password
```

The reset immediately revokes all sessions for that user. A successful self-service password
change also revokes all their sessions, including the current browser, and signs them out.
Passwords must have at least 16 characters and at most 1024 bytes; the CLI removes only trailing
newlines from password files. Passwords use per-user salted Argon2id hashes (19 MiB, two passes,
one lane). Grant and
role changes take effect on the next browser request after restart. When removing or renaming a
project, update declarative grants too. Keep the database in backups because current passwords
and sessions live there. There is no self-registration or browser account administration.

### Browser boundary

The reverse proxy must preserve the original `Host` header and serve HTTPS. Login uses an opaque
random session cookie with `Secure`, `HttpOnly` and `SameSite=Strict`, a 30-day absolute expiry,
and server-side session revocation at logout. Sessions persist in SQLite across orchestrator
restarts.

Mutations require a matching browser Origin and session CSRF token. Login attempts are globally
limited to 20 per minute. Browser cookies do not authorize the admin or worker APIs; API tokens
do not sign into the UI. Task output is escaped; instructions support safe Markdown and patches
have an inert diff view, with the original text still available. Browser actions record
`user:<id> (<display name at the time>)`. SSO and per-user API tokens are not yet available. The `/admin/v1` bearer
still has instance-wide authority and accepts caller-supplied actor labels.

`--ui-insecure-cookie` is exclusively for the loopback HTTP development stack. Production should
use the secure default behind HTTPS.

## Configuration layers and current support

| Section | Purpose |
| --- | --- |
| `instance` | Name, bot commit identity, local data directory and admin-token reference |
| `forges` | API root, optional browser root, bot login and controller-side credential reference |
| `model_endpoints` | Provider/auth mode and worker-side credential reference |
| `worker_pools` | The workers allowed to serve each pool, their tokens, and intended execution capabilities |
| `agent_profiles` | Harness/version constraint, model/settings, endpoint and pool |
| `projects` | Description, shared agent instructions, repository scope, profiles, delivery/effects, network policy, limits and optional discovery |

`artidezi schema config` exports the current schema. Configuration parsing rejects unknown fields
and validation checks cross-references and permission bounds. Validation alone does not prove the
chosen execution environment exists: Claude and several auth/sandbox modes remain modeled only.
Forgejo supports delivery; GitHub supports App-authenticated discovery, inspection and opt-in
COMMENT reviews and adaptation PRs. Explicit source policies can approve or merge unchanged
GitHub updates; release operations remain unavailable. Coding support includes Codex and Pi with the
implemented worker/harness capabilities. Pi consumes an OpenAI-compatible Chat Completions endpoint
and keeps its API key on the worker. See [worker internals](../architecture/worker-contract.md).

A forge may set `web_base_url` when its browser-facing root differs from `api_base_url` (for example,
`http://127.0.0.1:3081` versus `http://forgejo:3000` in a local stack). Repository and assessment-list
PR links use that root; it does not change controller API or Git access. Omit it to use the API root.

The server reads configuration and credentials at startup; there is no hot reload or configuration
editor in the UI. Validate a changed file before restarting. Model settings are harness-specific;
provision the pinned harness and needed test tools separately. The standalone worker uses its own
configuration containing orchestrator URL, worker token file, worker/pool IDs, workspace/credential
roots and optional binary paths. It does not need the orchestrator configuration or forge token.
For Codex `chatgpt_login`, enroll every worker independently and keep its credential directory
writable by the worker service. The worker copies `auth.json` into each fresh attempt and atomically
persists valid same-account token rotations afterward. Do not clone one enrolled `auth.json` across
workers; refresh-token rotation on one machine can invalidate the other copies.

## Resource limits

`instance.limits` bounds what workers can send to the orchestrator. Every value is optional; these are the defaults:

```yaml
instance:
  limits:
    max_upload_bytes: 1073741824         # one artifact
    max_attempt_upload_bytes: 4294967296 # all artifacts of one attempt
    max_concurrent_uploads: 4            # further uploads wait for a slot
    max_concurrent_inspections: 2        # worker results inspected at once, per pool
    max_bundle_bytes: 536870912          # one Git bundle from a worker
    git_fetch_timeout_seconds: 300       # all Git work on one worker result
```

Uploads are written to disk as they arrive, so memory use doesn't grow with their size. Raise the limits for repositories with large legitimate changes. An attempt above a limit is rejected or marked invalid, and its task needs a new attempt.

Requests are authenticated before their body is read. Requests other than uploads are limited to 32 MiB.

## State, backups and upgrades

The configured data directory holds SQLite, immutable artifacts and Git caches. Keep it on a local
filesystem. Back up the database together with its referenced artifacts, plus configuration and
separately managed secrets.

You can back up the data directory while the orchestrator runs. A copy of the live
`artidezi.sqlite` taken that way may be inconsistent, so the orchestrator also writes a consistent
copy to `backup/artidezi.sqlite` at startup and then hourly. The orchestrator never reads it. To
restore from it, stop the orchestrator, move it over `artidezi.sqlite` and delete
`artidezi.sqlite-wal` and `artidezi.sqlite-shm`. Tasks from the last hour are then missing.
Do not delete caches while tasks are outstanding: candidate/imported Git objects can be useful for
recovery even where published objects could be refetched.

Schema migrations run when the store opens. Preserve a pre-upgrade backup and finish or deliberately
hold outstanding work before operational changes. [Artifact retention](artifact-retention.md) can
expire completed work's frozen repository inputs and old unreferenced files. There is no
one-command restore/migration tool, multi-replica database support or independent verifier service.
Detailed persistence responsibilities are in [security and storage](../architecture/security-and-storage.md).

## Investigate stuck work

Large repository imports can take time. Browsing the UI and servicing worker leases/heartbeats do
not wait for those imports. Task-changing actions and forge operations still run in order; a scan,
new task or revision request can wait for preparation. If a request times out, inspect the task and
Incoming changes lists before submitting it again: a client timeout does not cancel server work.

| Symptom | First checks |
| --- | --- |
| Task stays queued | Correct pool, running worker, supported schema and installed harness version |
| Discovered PR waits | Author/branch match, draft state, settle window, checks, capacity and source hold |
| PR never re-evaluates | Already evaluated head, exhausted budget, needs-human/failure hold; inspect before explicit resume. A failed-checks delivery hold releases itself once checks pass or the head changes |
| Needs-human after a patch | Actual verification records, allowed paths, source freshness and publication ownership/conflicts |
| Logged out unexpectedly | Check session expiry, password reset, account disable or cleared browser cookies |

`/healthz` confirms the HTTP process responds; it is not an end-to-end worker/forge health check.
Use task/attempt evidence and service logs for the workflow. Avoid exporting tokens or full private
logs into public bug reports. A local real-model project can remain configured while its temporary
worker is disconnected; the standard scripted Compose worker cannot execute a Codex job.

A worker whose child does not exit within 10 seconds of SIGTERM followed by 5 seconds of SIGKILL
stops renewing the lease and accepting new work. Inspect its service journal and the
`worker-quarantine.json` marker under the configured workspace root; attempt inputs and partial
outputs remain there for diagnosis. A kernel-stuck process may require a worker VM reboot.
Recover the environment before removing the marker and restarting the supervisor. Restarting
only the supervisor does not clear quarantine. Another healthy worker in the pool can continue
queued tasks; a pool with one quarantined worker needs operator recovery.

The controller also enforces a fixed lease deadline derived from the frozen harness and recipe
timeouts, termination allowances and 30 minutes of processing overhead. Heartbeats cannot keep
an attempt alive forever or revive an expired lease. An existing result file
alone is not an accepted result and is never published automatically after a stall.

### Lost execution recovery and multiple workers

The default lease is two minutes without a successful heartbeat (`serve --lease-ttl-seconds`),
with an independent five-second recovery loop. Slow repository preparation or forge maintenance
does not hold up lease expiry. A second compatible worker can immediately take other queued work.
A supervisor that continues heartbeating while its child hangs instead relies on command timeouts,
bounded termination and the frozen overall execution deadline.

Automatic execution retries are opt-in per pool:

```yaml
worker_pools:
  maintenance:
    harness_sandbox: disabled
    harnesses: [codex]
    workers:
      worker-1: {token_ref: worker-1-token}
    lost_attempt_retries: 2
    transient_failure_retries: 5
```

Zero (the default) leaves lost work for a human; at most three retries are allowed. A replacement
starts fresh from the same frozen repository inputs, instructions and revision context. It does not
resume the old session or retry publication. Tasks with accepted results, unclassified execution records or publication history are
held for human inspection. Repeated loss beyond the configured limit also needs human attention.
The worker that lost its lease is held from new assignments for five minutes, allowing another
worker to take over. With one worker, recovery can wait for its cooldown and environment recovery.

`transient_failure_retries` separately allows up to five fresh executions after recognized terminal
Codex capacity, temporary rate-limit or connection errors with no result. It defaults to zero.
Retries wait 1, 2, 5, 10 and 30 minutes and retain the original frozen inputs. The task shows its
scheduled retry time; no comment or manual action is needed. If discovery already observed
a newer source head, recovery schedules a fresh evaluation within the existing source budget. It
does not rerun the obsolete snapshot. Authentication errors, exhausted
quota, invalid results, failed verification and exhausted retries still require attention.

Enable retries only for workflows that tolerate repeated and potentially overlapping execution.
Losing contact does not prove the original process or its detached containers stopped. The controller
rejects late results and owns publication, but cannot fence arbitrary external effects performed by
repository commands.

Run workers in the same pool with unique IDs and separate workspace/credential roots. One daemon
executes one attempt at a time. Separate VMs isolate kernel and Docker state; separate physical hosts
also preserve capacity during a host outage. Separate controller instances/pools do not back each
other up automatically. Give each Codex worker an independently authenticated session; do not clone
one rotating login across concurrent workers. See [OpenAI's runner-auth guidance](https://learn.chatgpt.com/docs/auth/ci-cd-auth).

The **Workers** page shows individual workers to instance administrators: last contact, current task,
lease expiry, quarantine, cooldown and drain status. Last contact proves supervisor communication,
not command progress. **Drain worker** stops new assignments while current work completes;
**Resume assignments** clears drain, while preserving quarantine/cooldown. Project readers retain
pool summaries without host identities or scheduling controls. These controls also use authenticated
`GET /admin/v1/workers` and `POST /admin/v1/workers/drain` with
`worker_pool`, `worker_id` and `drained` in the JSON body.

The configuration decides which workers exist. To add one, list it under its pool's `workers` with its
own `token_ref`, and restart the orchestrator; it shows as **Never connected** until it first calls in.
To remove one, delete its entry and restart: its token stops working and the orchestrator forgets it.
Any work it held is recovered like a lost attempt. A worker that is listed but dead stays listed as
offline.

For maintenance, **Pause automatic scheduling** on the Workers page stops the automatic forge,
discovery, CI-delivery and cleanup timer. It persists across restart and does not pause individual
sources, reset their budgets or block current worker result publication. Pair it with worker drain
when queued work must remain idle. Lease expiry and execution retry bookkeeping continue every
five seconds. Explicit manual actions remain available. Resume scheduling after maintenance.
The admin API exposes `GET /admin/v1/maintenance` and `POST /admin/v1/maintenance` with
`{"discovery_paused": true}` (or `false` to resume).
