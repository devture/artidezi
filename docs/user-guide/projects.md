<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Projects and instances

An **instance** is one administrative and credential boundary: an orchestrator, its configuration,
durable state, forge identities and associated execution workers. It can serve many projects.
A **project** is a reusable definition of a kind of work: repository inputs, allowed changes,
verification, network policy, profiles and delivery modes. A **task** is one request within it.
Do not create a new project for each issue, dependency bump or PR.

Use `description` for the short human-facing explanation shown in the UI and included as context for
the agent. Optional `agent_instructions` supplies trusted guidance shared by every manual and
discovered task in the project. Keep workflow-wide expectations there; put a discovery rule's
update-specific procedure in its handler `instructions`, and leave the concrete request or PR text
to the task layer. Project guidance cannot expand configured repository access, effects or other
controller-enforced policy.

The Projects page lists each project with its repositories, target branches, agent access and delivery mode.

## Choose the boundary

Use separate instances when different organizations or trust domains must keep credentials, source
code, artifacts and administration apart. Projects are policy scopes inside a shared instance;
they are not tenants, independent login realms or secret-isolation boundaries. UI accounts have
per-project grants or instance administrator access. Separate worker pools help route jobs to
appropriate environments, but do not turn a shared orchestrator into a multi-tenant service.

Related open-source repositories can share an instance and worker pool while keeping focused
projects. Repositories managed for a different organization may need a separate instance and workers.
Adding projects does not require one VM or one model credential per project. Different test tooling
or execution requirements can justify separate pools/profiles within an instance.

## Start with a writable target and deliberate context

Every repository in a project is snapshotted into every task. It has an explicit target branch and
`agent_access: read_write` or `read_only`. The latter supplies context and can support tests but
cannot contribute a published patch. Access declarations are validated output policy, not filesystem
isolation against a malicious process inside the worker.

Start with one writable target. Add a few read-only siblings, integration templates or API consumers
when their context materially helps the work. A prose path on an administrator's workstation is not
an input: a remote repository and branch must be configured so the worker receives its snapshot.
Shared deployment templates can provide context without including private operational configuration.

| Project shape | Writable input | Read-only context | Typical task |
| --- | --- | --- | --- |
| Shared library | Library source | Selected applications using its API | Fix a bug while preserving consumer compatibility |
| Application | Application source | Relevant libraries or API definitions | Adapt to a dependency update |
| Deployment configuration | Deployment source | Packaged service source and configuration examples | Adapt configuration after a service release |
| Coordinated migration | Component and consumer source | Relevant interface examples | Prepare an interface change and its consumer adaptation together |

Avoid granting write access to every related repository in one project: it expands each task's
scope, snapshot cost and available changes. Several small projects are useful even when the repositories
are related. The current system has no per-task repository-subset selector; prose cannot narrow
the actual granted access. Repository `allowed_paths` can constrain changed paths within a target.

## Prepare the repository's CI

ArtiDezi pushes each proposal to an `artidezi/<task>` branch in the target repository, and your forge's CI may run it before anyone reviews it. Treat those branches as untrusted, like a pull request from a stranger. The code comes from an agent that may have read hostile PR text or release notes.

Before enrolling a repository, check that CI on a non-default branch:

- gets no secrets, and no credentials beyond read access. Limit publishing, deploying and release jobs to the default branch or tags.
- doesn't run with a write-capable token. On GitHub, set `permissions: contents: read` in each workflow, or make read-only the repository's default.
- doesn't run on a self-hosted runner that holds credentials or other projects' work.

An adaptation can change any script or build file that CI runs, so a job's permissions matter more than its workflow file. Use `allowed_paths` to narrow what a project's agent may change.

ArtiDezi holds any proposal that changes the forge's workflow directories (`.github/workflows`, plus `.forgejo/workflows` and `.gitea/workflows` on Forgejo) until an administrator [authorizes that attempt](github-enrollment.md). On Forgejo and Gitea this hold is the only protection, because the bot's token can write workflow files. An authorized workflow can read any secret the repository can see, including organization-level ones, so review it first and keep secrets out of enrolled repositories and their organizations.

## Worker tags

Some projects need things only some workers have. Tags are free-form names with no built-in meaning, such as `docker-support` or `rust-support`.
A worker lists what it offers in its `tags`, and a project lists what it needs:

```yaml
projects:
  myapp:
    required_worker_tags: [docker-support]
```

A task goes to a worker in the profile's pool that has all the required tags, and it may have others.
Until one connects, the task stays queued and shows as **Unschedulable**, with the reason.
Tags are frozen with a task, so changing them affects new tasks only.

Tags only route work. Workers report their own tags, so a worker can claim any tag and receive that work from its pool.
To keep work away from less trusted workers, use a separate worker pool, and point the project's profile at it.

## Coordinated work and discovery ownership

A multi-repository task can produce one proposal per changed writable repository. Its configured
checks can use the sibling checkouts to test their combined behavior. Publication and merging across
repositories are not atomic; the reviewer must coordinate the related PRs.

For an occasional paired migration, a separate manual-only project can include two writable
repositories while routine projects retain narrower permissions. If assessing a shared component
regularly needs candidate changes in two consumers, its project can explicitly include all three
as writable targets, while watching only the component repository. Unchanged targets do not produce
proposals. This wider permission is a deliberate configuration choice. Discovered assessments
retain candidates in default dry-run mode or publish human-reviewed proposals when explicitly enabled.

Share context where useful, but have only one automatic discovery owner for a forge repository and
target branch. Configuration validation rejects overlapping watched ownership. A manual-only project
does not compete for that ownership, although humans should still avoid overlapping edits to the
same targets.

When the upstream is a versioned dependency, prefer its release as the boundary between tasks:
upstream change/release first, then a dependency updater's consumer PR. The consumer assessment must
include the real dependency bump and any integration adaptation together. For application containers,
wait for the actual published image, not just a source tag. Upstream tasks can use read-only consumers
to anticipate migration requirements; broader writable scope is useful for optional preparation.
See the [release-boundary proposal](../architecture/project-composition.md#default-delivery-across-a-release-boundary)
for Git dependency, library package and container examples. This delivery chain is not yet implemented
end to end in ArtiDezi.

## Profiles and activation

Projects allowlist agent profiles and delivery modes and choose defaults. Profiles select a harness,
model endpoint and worker pool. Reuse those definitions across projects. The Agent profiles page
shows which projects use each profile by default and which merely allow it. An incoming-change handler
may select an optional `agent_profile` from its project's allowlist; otherwise it uses the project
default. This is useful for a bounded low-risk update class without weakening manual or unrelated
tasks in the same project.

Manual tasks work without automatic discovery. Opt-in `pull_requests` names the watched repository
IDs; read-only context is not automatically watched. Use exact author handlers for the updater
accounts you want. A catch-all is never required. See [Incoming changes](pr-discovery.md).

Changing configuration requires a server restart. Existing tasks keep their frozen policy; a new
manual task resolves the new policy, while a manual revision retains its original policy. A new
source-PR follow-up resolves current configuration and current inputs. Removing or repurposing IDs
can disrupt unfinished tasks and source tracking, so finish/hold that work before changing ownership.

## Local experiments are different from deployment projects

A fresh local stack has only `playground`. Additional entries come from an operator's optional local
override. Keeping separate compatible/breaking or one-/two-repository fixtures makes regressions
reproducible; those scenario names should not become the permanent production project structure.
Keep their history while reviewing experiments, then define stable project names around real work.

## Reusing definitions across related projects

Current configuration repeats repository fields at each project binding. A small shared repository
catalogue is [proposed](../architecture/project-composition.md) to remove that repetition while
keeping access explicit at each binding. It is not implemented YAML syntax. “Related project”
relationships that inherit permissions or trigger tasks recursively are not needed for this model.
