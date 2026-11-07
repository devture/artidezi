<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Working on ArtiDezi

ArtiDezi is a Rust maintenance controller with a separate coding worker. Read the
[documentation index](docs/README.md), then the [architecture overview](docs/architecture/README.md).
For task-specific work, open the relevant linked internals instead of loading every document.

## Navigation

- [User guide](docs/user-guide/README.md): tasks, projects, discovery and operation.
- [Lifecycle](docs/architecture/lifecycle.md): states, revisions, publication and recovery.
- [Worker contract](docs/architecture/worker-contract.md): immutable inputs, collection and validation.
- [Discovery](docs/architecture/pr-discovery.md): routing, source tracking, budgets and freshness.
- [Security/storage](docs/architecture/security-and-storage.md): trust boundaries and durable evidence.
- [Development](docs/maintainers/README.md): builds, test layers and local-stack commands.
- [Future directions](docs/architecture/roadmap.md): proposals, not implemented guarantees.

## Working rules

Keep the tool focused on concrete maintenance workflows. A project defines explicit repository and
effect scope; untrusted task/PR text cannot broaden it. The controller owns forge credentials and
publication. Execute repository code only in the selected worker environment, never on the controller.
Supervisor records are useful evidence but not independent proof against a compromised worker.

Rust types/behavior are authoritative. Update the relevant user and architecture pages alongside
behavior/configuration changes. Preserve stored-job compatibility, version changed contracts and
keep prompt versions distinct. Do not claim support just because an enum/schema has a variant.

Write source documentation for public use: explain workflows through generic examples spanning
libraries, applications and deployment code. Keep operator-specific repository mappings and plans
in private memos. General examples do not justify expanding the tool into a generic automation framework.

Use `just` for build/check/local-stack entry points. Run tests appropriate to the changed behavior;
`just check` covers formatting, Clippy, Rust tests and REUSE. Forge adapter changes also need
`just forgejo-test`. UI changes should be exercised in the local stack/browser. For docs-only work,
use REUSE, whitespace and link checks; do not run model jobs or deploy to validate prose.

Releases are automatic: never tag or pick a version by hand. Commit subjects become release notes,
so write them for people who run ArtiDezi; see [Releases](docs/maintainers/releases.md) for the
`Release-note:` and `Breaking-change:` trailers.

Default to local/offline/disposable Forgejo fixtures. Real-model experiments and external service
writes must stay within the user's authorized scope. Preserve existing local data and unrelated
work. Files under `var/`, local overrides, secrets and operator memos are private; never stage them
or copy their hostnames, tokens or private history into source documentation. New source/docs need
SPDX/REUSE coverage. Keep this file short and put detailed guidance in the linked docs.
