<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# User guide

ArtiDezi lets an administrator submit maintenance tasks and review their resulting patches. It can
also discover selected pull requests and record an assessment. A project supplies the context and
permissions; the coding agent does not choose its own access.

1. [Try the local stack](../maintainers/README.md#try-the-whole-stack-locally) with a scripted worker.
2. [Install the orchestrator and a worker](installing.md).
3. [Choose projects and instance boundaries](projects.md).
4. [Configure the instance and authentication](operations.md), including [artifact retention](artifact-retention.md).
5. [Submit tasks, review evidence and request revisions](tasks.md).
6. [Optionally configure Incoming changes](pr-discovery.md).

Current support: Forgejo delivery, GitHub App intake, Codex, Pi and a scripted test harness; a
web UI with named accounts and per-project grants; manual single- and multi-repository proposals; evaluation of existing
PRs; and read-only GitHub PR inspection. SSO, chat integration and automatic issue
intake are not implemented.

For GitHub, start with [read-only inspection and enrollment](github-enrollment.md).
