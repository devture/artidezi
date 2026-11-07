<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Artifact retention

Each attempt freezes repository history into Git bundles. Exact-file deduplication shares identical
bundles, but successive snapshots can still accumulate large copies of Git history. Configure
retention to keep a short reproduction window after completion without losing review evidence:

```yaml
instance:
  # Other instance settings omitted.
  artifact_retention:
    completed_task_bundle_retention_seconds: 86400
    orphan_grace_seconds: 86400
    cleanup_interval_seconds: 3600
```

All durations must be positive. This example keeps completed tasks' frozen repository inputs for
24 hours and runs cleanup hourly. Omit `artifact_retention` to disable cleanup, preserving existing
installation behavior. Automatic cleanup first runs one configured interval after controller startup;
an administrator can preview or apply it immediately.

Only `done` and `closed` tasks become eligible, measured from their last task update. Unfinished
attempts, tasks waiting for CI or review, human holds and any recorded open published proposal
protect their inputs. Before collection, the controller refreshes recorded open proposals of retired
tasks through read-only forge queries. It records observed closure/merge states without merging,
publishing, creating tags or changing task states. Failed reads leave the proposal protected and
appear in the cleanup report. Shared inputs still referenced by protected work remain available.

Expired files are the job's repository snapshot and PR input bundles. Results, review/audit history,
prompts, immutable job and attempt documents, patches, logs and worker-produced commit bundles
remain. Worker bundles may contain unpublished changes, so retaining them avoids treating the
forge as their only backup. The collector also removes unreferenced completed artifacts after the
orphan grace period and protects uploads awaiting acceptance, including fresh uploads of old
deduplicated content. Git caches and temporary staging files are not cleaned by this policy.

Preview the configured policy through the authenticated admin API or CLI:

```sh
artidezi task --url https://maintenance.example.org --token-file /secure/admin-token cleanup-artifacts
artidezi task --url https://maintenance.example.org --token-file /secure/admin-token cleanup-artifacts --apply
```

`POST /admin/v1/artifacts/cleanup` accepts `{"apply": false}` (also the default) for a dry run and
`{"apply": true}` to delete. The report lists eligible tasks, candidate hashes/sizes/reasons,
reclaimable bytes and bytes actually removed. Dry runs may refresh retired publication observations,
but do not mark tasks expired or delete files.
Application recomputes reachability under the controller operation lock; it does not trust an old
dry-run plan. Cleanup requires a configured policy and never accepts a replacement policy from
task text or workers. Failures in retained evidence abort the mark phase before deletion.

The attempt page identifies intentionally expired repository inputs. Exact offline replay is no
longer available after expiry, and upstream repositories may eventually discard old objects too.
New evaluations obtain fresh repository inputs; retained patches and worker bundles continue to
describe unpublished changes. Expiry persists even if a terminal task is later updated or the
retention window is lengthened. Disabling retention stops future collection, but does not restore
removed files. Keep a consistent database/artifact backup if exact long-term replay is required;
external backups have their own retention policy.
