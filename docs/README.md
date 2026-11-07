<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# ArtiDezi documentation

Start with the [user guide](user-guide/README.md) to operate an instance, the
[architecture overview](architecture/README.md) to understand its design, or the
[development guide](maintainers/README.md) to change and verify the code.

| Audience | Guide | What it covers |
| --- | --- | --- |
| Operators | [Installing](user-guide/installing.md) | The image, the worker binary and a first deployment |
| Operators | [Projects and instances](user-guide/projects.md) | Repository grouping, context, permissions and shared workers |
| Operators | [Tasks and reviews](user-guide/tasks.md) | Manual work, revisions, evidence and dry-run evaluations |
| Operators | [GitHub enrollment](user-guide/github-enrollment.md) | App setup, dry runs and scoped publication |
| Operators | [Incoming changes](user-guide/pr-discovery.md) | Optional matching, polling, reactivation and holds |
| Operators | [Configuration and operation](user-guide/operations.md) | Configuration, credentials, UI authentication and durable state |
| Operators | [Artifact retention](user-guide/artifact-retention.md) | Completed-task input expiry, orphan cleanup and preserved evidence |
| Developers | [Architecture](architecture/README.md) | Boundaries, data flow and source map |
| Developers | [Lifecycle](architecture/lifecycle.md) | Task/attempt states, publication and recovery |
| Developers | [Worker contract](architecture/worker-contract.md) | Immutable inputs, execution, collection and validation |
| Developers | [PR discovery internals](architecture/pr-discovery.md) | Routing, scheduling, source tracking and freshness |
| Design | [Project composition proposal](architecture/project-composition.md) | Reusable repository definitions and adaptations across components and consumers |
| Operators and developers | [Web access](architecture/web-access.md) | Local user identities, project permissions and future SSO |
| Developers | [Security and storage](architecture/security-and-storage.md) | Trust assumptions, credentials, persistence and verification limits |
| Maintainers | [Development and local stack](maintainers/README.md) | Builds, test layers, UI iteration and documentation maintenance |
| Maintainers | [Releases](maintainers/releases.md) | Automatic CalVer releases, published images and binaries, release notes |

These pages describe the current source checkout unless explicitly labelled as future work.
[Future directions](architecture/roadmap.md) are separate from working features. Rust types and
behavior are authoritative; keep the relevant guide updated in the same change as the code.
Deployment-specific inventories, credentials and private experiment history belong outside these docs.
