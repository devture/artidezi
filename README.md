<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

<p align="center"><img src="assets/branding/artidezi-bot.svg" width="128" alt="ArtiDezi logo"></p>

# ArtiDezi

**The maintenance agent that pushes the boring buttons.**

ArtiDezi is a self-hosted tool which lets coding agents do routine maintenance on your repositories, without giving them your repo credentials.

Point it at pull requests from an updater such as [Renovate](https://docs.renovatebot.com/), or describe a task yourself.
A coding agent ([Codex](https://github.com/openai/codex) or [Pi](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)) does the work, and you get one of these on your forge ([Forgejo](https://forgejo.org/) or [GitHub](https://github.com/)):

- ✅ **Merge as is**: the change is fine as-is, and ArtiDezi auto-merges it
- 🔧 **Adaptation**: the change needs more work, so ArtiDezi proposes a PR for you to review
- 🙋 **Needs a human**: ArtiDezi stops and tells you why

## How it works

ArtiDezi has 2 parts, so that the *untrusted worker* agent doesn't get near your forge credentials or be allowed to merge stuff by itself:

- The **orchestrator** is trusted. It holds the forge credentials and is the only part that publishes anything.
- The **worker** is not. It runs the coding agent on an expendable machine, and only hands back its verdict and changes.

You decide what the agent may touch in each [project](docs/user-guide/projects.md).
A compromised worker can lie about what it did, so [human review remains the safeguard](docs/architecture/security-and-storage.md).

> [!WARNING]
> For simplicity, the worker isn't a sandbox yet: it's reused from task to task. A rogue agent can read the harness credentials stored on it.

> [!TIP]
> Give each worker a machine and credentials you can afford to lose, and use [separate pools](docs/user-guide/projects.md#worker-tags) for projects that shouldn't mix.

## Examples

Here are 2 real examples from the [matrix-docker-ansible-deploy](https://github.com/spantaleev/matrix-docker-ansible-deploy) project, where [Renovate](https://docs.renovatebot.com/) proposes updates for dozens of services:

- ✅ **A scary upgrade, merged as is.** [Mjolnir v1 to v2](https://github.com/spantaleev/matrix-docker-ansible-deploy/pull/5648) deleted a whole mode of operation. ArtiDezi read the upstream diff, confirmed that the playbook only uses the mode that stayed, [published its evidence](https://github.com/spantaleev/matrix-docker-ansible-deploy/pull/5648#pullrequestreview-5538063524) along with what it hadn't tested, and merged the PR about 2 minutes after picking it up.
- 🔧 **A green PR that would have broken things.** [matrix-user-verification-service v4](https://github.com/spantaleev/matrix-docker-ansible-deploy/pull/5707) was a one-line bump with passing CI, but upstream had quietly stopped reading the `.env` file, without a word in its changelog. The service would have started healthy while ignoring its configuration, leaving its API without authentication and denying Matrix logins to Jitsi. ArtiDezi [proposed an adaptation PR](https://github.com/spantaleev/matrix-docker-ansible-deploy/pull/5708) that passes the configuration through Docker, and a maintainer merged that instead.

## Try it

With Docker Compose v2, Python 3.11+ and [`just`](https://just.systems/):

```sh
git clone https://github.com/devture/artidezi.git
cd artidezi
just services-start
just services-credentials
```

Open <http://artidezi.127.0.0.1.nip.io:3080>: a private Forgejo, the orchestrator and a scripted worker, with no model calls.
See [the local stack](docs/maintainers/README.md#try-the-whole-stack-locally).

## Installation

The orchestrator is a container image, [`ghcr.io/devture/artidezi`](https://github.com/devture/artidezi/pkgs/container/artidezi).
The worker is the same program as a static binary, attached to each [GitHub release](https://github.com/devture/artidezi/releases).
See [Installing](docs/user-guide/installing.md).

## Documentation

Start at the [documentation index](docs/README.md), or go straight to a topic:

- 📦 [Installing](docs/user-guide/installing.md): the orchestrator, the worker and their first run
- 🗂️ [Projects and instances](docs/user-guide/projects.md): what to group, and preparing a repository's CI
- 📝 [Tasks and reviews](docs/user-guide/tasks.md): requesting work, reading evidence, asking for revisions
- 📥 [Incoming changes](docs/user-guide/pr-discovery.md): assessing pull requests from updaters and people
- 🐙 [GitHub enrollment](docs/user-guide/github-enrollment.md): the GitHub App, dry runs and publication
- ⚙️ [Configuration and operation](docs/user-guide/operations.md): settings, accounts, backups, troubleshooting
- 🏛️ [Architecture](docs/architecture/README.md): the design, and what each boundary does and doesn't guarantee
- 🧑‍💻 [Development](docs/maintainers/README.md): building, test layers and the local stack

## The name

The name ArtiDezi comes from **Arti**ficial [**Des**mond](https://lostpedia.fandom.com/wiki/Desmond_Hume): an AI that's [pushing the boring buttons](https://lostpedia.fandom.com/wiki/Pushing_the_button) for you.

## License

[Apache-2.0](LICENSES/Apache-2.0.txt)
