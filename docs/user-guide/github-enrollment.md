<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev
SPDX-License-Identifier: Apache-2.0
-->

# Preparing GitHub enrollment

ArtiDezi supports GitHub.com inspection, App-authenticated scheduled evaluations, and opt-in
publication of reviews and adaptation PRs, plus opt-in approval and merging of unchanged updates.
Manual tasks can be submitted for every configured project. For them to publish on GitHub, the project
needs both `effects.publish_adaptation_merge_requests` and `effects.allow_manual_github_publication`.

## Configure a binding and check access

Start with the [complete example](../../etc/github-dry-run.yaml), replacing its placeholder App,
repository and worker settings. One forge binding identifies one App installation/account:

```yaml
forges:
  github-example:
    provider: github
    api_base_url: https://api.github.com
    web_base_url: https://github.com
    credential_ref: github-app-key
    bot_login: example-maintenance[bot]
    github_app:
      client_id: Iv.REPLACE_ME
      installation_id: 12345
      account: example
```

Store the complete unencrypted RSA PEM in the controller's secrets directory as `github-app-key`,
readable only by the service and its administrator. Client ID, installation ID and bot login are
non-secret configuration. The installation account may differ from the account that registered the
App. The controller signs App JWTs and renews installation tokens automatically; each token is
restricted to one configured repository. Reads always request the five read permissions below;
publication uses a separate token with Contents and Pull requests write.

Before starting or restarting discovery:

```sh
artidezi validate-config artidezi.yaml
artidezi github-check --config artidezi.yaml --secrets-dir /etc/artidezi/secrets --forge github-example
```

`github-check` verifies App identity, the installation account, configured permissions and
access to every repository using that binding, including write-token scope for publication-enabled
repositories. It prints repository names and the preflight result, never keys,
JWTs or tokens. It does not open SQLite, start a worker or schedule evaluations. Its only POST
issues a short-lived installation credential; it performs no repository mutation. A successful
preflight does not prove worker provisioning or application compatibility.

Omit the project's `pull_requests` block while checking access. Add it when ready for scheduled
evaluations and restart the controller. The UI's **Incoming changes** page then shows observed PRs
and links to their evaluation tasks. Nothing appears merely from adding an App: discovery requires
an explicit repository, matching handler and eligible source. No catch-all is required.

## Inspect a candidate

Build with `just build-dev`, then run:

```sh
target/debug/artidezi github-inspect --repository example/application --pull-request 123
```

The command prints JSON and exits. It does not connect to an ArtiDezi instance, create an evaluation,
invoke an agent or write to GitHub. Without `--token-file`, it uses anonymous public API access and
ignores ambient `GH_TOKEN`, Git credentials and CLI login state. Public requests share GitHub's
unauthenticated rate limit, currently 60 requests per hour per source IP; this is suitable for a
small inspection, not sustained polling. An ordinary one-file PR with at most 100 statuses/checks
uses six API requests. [GitHub rate-limit documentation](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)
explains authenticated quotas and secondary limits.

For private repositories or an authenticated probe, explicitly pass `--token-file /secure/read-token`.
Use a token limited to selected repositories and read permissions. An already-issued GitHub App
installation token also works, but this inspection command does not mint or renew it; the configured
runtime adapter and `github-check` use App authentication instead. Keep private reports and
credentials out of source control. Tokens and server error bodies are excluded from output.

The report separates legacy commit statuses from check runs (including GitHub Actions). It treats
missing/incomplete/unknown checks conservatively and reads all advertised pages, up to 1,000 records
per category. Duplicate records, incomplete listings and PR movement during inspection fail instead
of returning a partial success. Redirects are refused; use the current canonical repository name
when a repository moves. API patches can be absent/truncated and are not worker Git snapshots.

`checks_summary: success` describes the returned check snapshot. It does **not** mean required
checks, review policy, branch protection or compatibility have been verified. A skipped job does not
prove the corresponding tests ran. GitHub also limits which check suites this endpoint exposes;
automatic merge gates need a separate required-check policy. See the API references for
[commit statuses](https://docs.github.com/en/rest/commits/statuses#get-the-combined-status-for-a-specific-reference)
and [check runs](https://docs.github.com/en/rest/checks/runs#list-check-runs-for-a-git-reference).

## Prepare an App

Use read permissions for dry-run enrollment, or the publication column when preparing for live effects. An operator owns registration and installation, then
supplies the controller's private-key file. Registration alone does not activate discovery.

| Repository permission | Dry run | Publication | Purpose |
| --- | --- | --- | --- |
| Metadata | Read | Read | Repository identity and availability |
| Contents | Read | Write | Target commits, source refs and Git snapshots |
| Pull requests | Read | Write | Authors, descriptions, branches and source state |
| Workflows | None | Conditional write | Only a separately authorized source proposal that changes `.github/workflows` |
| Actions | Conditional read | Conditional read | Only automatically selected failed-job log excerpts when enabled |
| Checks | Read | Read | GitHub Actions and other check runs |
| Commit statuses | Read | Read | Traditional CI status contexts |

Prefer **Only select repositories** when installing. **All repositories** is also supported: ArtiDezi
still issues tokens for one explicitly configured repository at a time and verifies their returned
scope. Installation access does not automatically enroll repositories or schedule work. The App key
itself has authority across the wider installation, including future repositories when All repositories
is selected. Add context repositories explicitly if the worker needs their snapshots.
Organization/account permissions, Administration and Issues are not needed. Actions read is needed
only when a project and handler explicitly enable selected CI job logs; it is separate from Workflows
write and from the ordinary five-permission read token.
An installation with write permissions can run dry-run-only projects: read tokens stay narrow and
publication requires explicit configuration. Workflow file changes are withheld; ordinary publication doesn't
request Workflows permission. A reviewed source proposal can use a
task-specific authorization below; its push requests a separate one-repository token with Workflows
write, which requires that permission on the App installation. The endpoint-specific
[permission reference](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
is authoritative.

Start with polling. Disable the App's webhook **Active** checkbox, do not request OAuth user
authorization during installation, and leave user callback/setup flows unused. A loopback UI does
not need a public webhook endpoint. Record the App/client identity, installation ID, bot login and
private-key file location; do not put the private key in project YAML, worker files or prompts.
These choices follow GitHub's [registration guide](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app).

An App restricted to its owning account cannot cover repositories in other organizations. For a
multi-account instance, decide between separate owner-scoped Apps or one App installable by **Any
account**, with explicit installation and repository allowlists. That visibility choice does not
require a Marketplace listing or making the ArtiDezi UI public. See
[App visibility](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/making-a-github-app-public-or-private).

Use a separate App for each independently administered ArtiDezi deployment. One deployment's App
can serve several GitHub accounts through separate installations. Every private key for an App can
authenticate across its installations; issuing different keys for personal and business deployments
does not separate their access. The controller's configured repository scope does not restrict what
a stolen App key can authorize. See GitHub's [credential guidance](https://docs.github.com/en/apps/creating-github-apps/about-creating-github-apps/best-practices-for-creating-a-github-app#private-keys).

Moving the same deployment to a new host does not require a new App. When projects move to a
separately administered instance, enroll them under that instance's App and disable their old
discovery rules before activating the new ones. Keep instance data separate too: changing App
credentials does not remove existing task history, logs or repository artifacts from the database
and artifact store.

Installation access tokens expire after one hour. The controller renews them with a two-minute
margin, verifies returned scope, and keeps App/Git credentials away from the worker. See [installation authentication](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation).
Do not use a pasted installation token as a permanent deployment secret.

## Roll out one effect at a time

| Stage | Forge effects | Evidence needed before progressing |
| --- | --- | --- |
| Inspect (implemented) | GET only | Correct source identity, CI visibility and repository ownership |
| Scheduled dry run (implemented) | Authenticated reads and local evaluations | Verified installation, working worker profile, correct author rules and reviewed dry-run outcomes |
| Publish (implemented) | COMMENT reviews and owned adaptation branches/PRs; human merge | Freshness and retry reconciliation; reviewers accept the proposed changes |
| Approval (implemented) | APPROVE review on an unchanged source | Deliberate author selection, successful current CI, reviewed pilot outcomes; may trigger another bot's automerge |
| Source merging (implemented) | Approval followed by an immediate merge of a PR from a named author | Project merge permission, source freshness, successful current CI and repository protection |

Keep dry run available per project or rule. Do not make a catch-all
mandatory. Start with one dependency bot and a few explicitly selected repositories; a reasonable
initial proposal is ten-minute polling, a short settle window, one active evaluation per project and
three evaluations per source. The example uses those values; they are not new global defaults.

While observing, leave existing updater and notification policies alone. Before ArtiDezi becomes
responsible for merging selected dependencies, remove the updater's competing automerge path **for
those dependencies only**. Changes requiring data migrations or an operator decision should request
human input, even if the code patch is small and CI passes.

For components consumed elsewhere, let the existing release process finish first. Only then evaluate
the consumer's actual dependency bump against the published version. Do not pre-bump consumers to
unreleased tags, create duplicate releases, or equate paired PRs with an ordered release chain.

## Operational limits

The adapter combines visible commit statuses and check runs. `wait_for_checks_to_finish` waits for
all reported checks to finish before assessment, whether they pass or fail. An empty check set is
distinct from running checks and permits assessment after the settle window. An all-skipped/neutral
set also permits assessment but does not satisfy automatic source-action requirements. ArtiDezi
does not inspect branch protection, required-check producers or review policies. Passing CI does
not establish migration safety.
Configure worker setup/verification commands appropriate to each repository; the example intentionally
has none. Unchanged-source verdicts do not automatically run verification recipes: the agent must
report any checks it actually performs and their limits.

Conditional GETs reduce repeated API transfer and quota use. Source/check listings are bounded to
1,000 entries; incomplete or inconsistent responses fail. Renamed repository redirects are refused;
update the configured identity. Git URLs stay on GitHub.com and Git redirects cannot forward
credentials. GitHub Enterprise and arbitrary custom API endpoints are not supported.

The controller persists server-advised backoff across restarts. **Scan now** cannot bypass it.
Authentication/permission failures are visible as scan errors, never fabricated source closure.
Preparation failures may pause a source; inspect it before resuming. A rate-limited preparation can
retry on a later eligible scan. If freshness cannot be checked after a completed worker result, the
task requests human input rather than treating stale evidence as current. Publication failures pause
the source; use the explicit retry described below. Eligible unchanged approvals/merges with pending
CI instead wait automatically, as described under [safe updates](#approve-or-merge-safe-updates).

The [integration internals](../architecture/github-integration.md) describe the boundaries and
publication recovery rules.

## Enable publication

After reviewing dry-run outcomes, change the selected handler to `mode: publish` and set the
project's `effects.publish_adaptation_merge_requests: true`. Omitted `mode` means `dry_run`.
Keep `safe_verdict_action: comment` (the default) and
`merge_source_merge_request_on_safe_verdict: false` until enabling the actions below. Recheck the installation, restart and
use **Evaluate again** on the current source task for a fresh evaluation. Existing dry-run tasks
never gain write authority from a configuration change.

Publish mode has three useful outcomes:

- `approve_untouched`: post an informational COMMENT review; no GitHub approval or merge.
- `proposal`: publish an ArtiDezi-owned adaptation PR per changed repository, then link them in a
  COMMENT review on the source. The original request remains open. Humans review/merge proposals
  and resolve the original request. ArtiDezi observes those merges without tagging or releasing.
- `needs_human`: publish the assessment as an informational review and pause for a decision.
  `failed` and invalid worker results publish nothing.

Published reviews keep the verdict, summary, agent-assessed risk and maintainer questions visible.
The complete evidence list is retained in a collapsed disclosure so a routine review stays compact
without discarding complex-task evidence. Inline code, identifiers and ordinary HTTP(S) Markdown
links remain readable; raw HTML in evidence notes is displayed as text, and mentions are neutralized.

The source head, eligibility, target/context commits, repository identity and configured write
scope are checked before each effect. Publication is not atomic across Git and multiple API calls:
a branch or PR may exist when a later step fails. The task then needs attention. **Retry publication**
(or `POST /admin/v1/tasks/{id}/retry-publication` with `{"actor":"admin"}`) uses the same validated
result and checks existing receipts before writing; it does not run another worker. Closed, edited,
foreign or moved proposals are not overwritten or duplicated. A restart during delivery exposes
the same retry action. A stale source/base requires a fresh evaluation instead.

When a validated proposal changes one of the forge's workflow directories (`.github/workflows` on
GitHub)—for example, a library test matrix, application build, or deployment verification job—ordinary
publication and ordinary retry stop before the first push. This also applies to manual merge-request
tasks; other manual delivery modes stay held and need a new attempt. Review the exact attempt and patch, then use the admin API to
authorize that attempt and retry it:

```http
POST /admin/v1/tasks/{task_id}/authorize-workflow-publication
Content-Type: application/json

{"attempt_id":"...","attempt_record_sha256":"...","reason":"Reviewed the CI workflow change for this update","actor":"maintainer"}
```

Read `attempt_id` and `attempt_record.sha256` from `GET /admin/v1/tasks/{task_id}`. The actor and
reason are recorded in `publication.workflow_authorized`, bound to that immutable attempt record.
The endpoint immediately uses normal publication retry; it does not run the worker again. A wrong
attempt or digest is rejected. A later evaluation needs its own authorization. If a forge response
is lost after the grant, use ordinary **Retry publication** to reconcile the existing branch and PR.
The controller still checks current source, repository scope, allowed paths, configured publication
effects and proposal ownership before effects. The App must have Workflows write permission for the
selected repository, but ordinary source evaluations continue to receive tokens without it.

**Evaluate again** and **Resume evaluations** cannot bypass an existing open adaptation proposal,
including one whose create response was lost. Resolve those PRs first. ArtiDezi does
not rewrite an existing adaptation PR from source-task feedback. Comments are not commands.


## Approve or merge safe updates

An assessment, a formal approval and a merge are different operations. The `approve_untouched`
result says the source needs no adaptation. By default it produces only a COMMENT review. A handler's
`safe_verdict_action` chooses what a qualifying unchanged result may do:

| Value | Action |
| --- | --- |
| `comment` (default) | Publish an informational review |
| `approve` | Submit a GitHub APPROVE review; another bot may then merge |
| `merge` | Submit an approval, then request an immediate merge commit |

These actions require `mode: publish`, the project publication effect and writable source
repositories. Forgejo supports the same actions; see
[approve or merge safe updates](pr-discovery.md#approve-or-merge-safe-updates). `merge` additionally requires
`effects.merge_source_merge_request_on_safe_verdict: true`. A merge handler merges pull requests
from the authors it matches, whoever they are: a bot such as `renovate[bot]`, or a person you trust
that far. Listing an author is the decision to merge their unchanged PRs without a person; an
`author_regex` or `catch_all: true` extends it to every login the selector matches, which on a
public repository can include anyone.
For example, within an existing enrolled project's handler:

```yaml
id: renovate
authors: ['renovate[bot]']
action: assess
mode: publish
safe_verdict_action: merge
wait_for_checks_to_finish: true
require_successful_checks: false
instructions: >-
  Review compatibility and run relevant checks. Propose adaptations when needed;
  report uncertainty honestly and request input when evidence is insufficient.
```

`wait_for_checks_to_finish` controls whether assessment waits for reported CI to finish; it defaults
to true. `require_successful_checks` retains a separate successful-check freshness gate when
publishing the assessment. Approval and merging **always** require successful current forge checks,
even when an assessment was allowed after failed or absent CI. They require an unchanged accepted
result and no outstanding change-request review. Pending CI
puts an otherwise eligible result in **Waiting for CI**. Risk ratings, evidence and questions are recorded in the task as worker-supplied context; they do not
replace the controller's forge, freshness or authority checks.

Adaptations remain human-reviewed PRs even under a merge handler, unless the handler separately sets
`adaptation_action: merge` (see [merge ArtiDezi's own adaptations](pr-discovery.md#merge-artidezis-own-adaptations)).
ArtiDezi does not change repository protection, enable GitHub auto-merge, use a GitHub merge queue
or create release tags. If merge commits are disabled, or repository rules
reject the operation, publication stops for attention; there is no fallback merge method or bypass.

The merge request sends the assessed head SHA to GitHub. The controller also rechecks target/context
commits before effects. GitHub does not expose an atomic expected-base parameter for this endpoint,
and approval has no atomic head guard. Configure required CI, stale-approval dismissal and up-to-date
branch rules as appropriate, and keep the App outside protection bypass lists. Repository rules
provide the final guard against races after the last read. See GitHub's
[merge endpoint](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request) and
[review endpoint](https://docs.github.com/en/rest/pulls/reviews#create-a-review-for-a-pull-request).

Choose one owner for automatic delivery: normally Renovate plus `approve`, or ArtiDezi plus `merge`.
Changing policy never promotes a stored comment-only or dry-run task; start a fresh evaluation.
**Waiting for CI** resumes automatically using the same accepted result. The controller checks every
2 minutes, subject to maintenance scheduling and forge backoff, without holding a worker or starting another evaluation.
The wait and its next check survive restarts. Before approval/merge it repeats source/context
freshness, review and authority checks. Pausing evaluations or closing the task stops automatic
delivery. Failed checks, changed inputs or permissions, and reviewer objections need attention;
subsequent green checks do not silently release those holds. Missing/all-skipped CI remains waiting
until usable checks arrive or the task is paused/closed. Forge rate-limit backoff is respected.

**Retry publication** remains available for interrupted delivery and other held valid results.
Held tasks don't enter CI waiting by themselves; an explicit retry may, if pending CI is then the
only blocker. A failed controller gate needs inspection or a new
assessment. A lost API
response is reconciled against the exact bot-owned approval. A planned approval with no matching
receipt requires inspection and a fresh evaluation; retry does not recreate a potentially deleted
review. An already merged source is recorded
without sending another merge or claiming who merged it when that cannot be established.
