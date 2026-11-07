<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Development and local verification

Start with [architecture](../architecture/README.md) and open only the internals relevant to the
change. The root [AGENTS.md](../../AGENTS.md) is the short navigation/working guide for coding agents.

## Try the whole stack locally

Requirements: Docker with Compose v2 and BuildKit, Python 3.11+, and `just`. Rust compiles inside the
build container. The initial build takes longer; subsequent builds reuse Cargo's cache.

```sh
just services-start
just services-credentials
```

Open **http://artidezi.127.0.0.1.nip.io:3080** and sign in as `local-admin` with the generated UI password. The local Forgejo is at
**http://artidezi-forgejo.127.0.0.1.nip.io:3081**, with its own generated login. All ports bind to loopback.
The distinct hostnames help password managers distinguish the UI and Forgejo logins. `nip.io`
resolves these names to `127.0.0.1`; no local reverse proxy is needed. Direct `127.0.0.1` URLs
on the same ports still work. For offline DNS, you can pin these names to `127.0.0.1` in `/etc/hosts`.
Switching from the IP address to the hostname requires a login because browser cookies are host-specific.

This starts the UI/orchestrator, a private Forgejo with an initialized `artidezi/playground`, and a
scripted worker. No personal tokens, model subscriptions, production repositories or remote
ArtiDezi deployment are used. The worker has its own network, with outbound access but no Forgejo connection, and no
Docker socket. Its only service credential is its local worker token.

To exercise the real Pi worker locally, place an OpenAI-compatible API key in the ignored file
`var/services/credentials/venice/api_key`, then start with `ARTIDEZI_LOCAL_HARNESS=pi
just services-start`. This selects the Pi/Venice example configuration and keeps the key mounted only
in the worker. Set `ARTIDEZI_LOCAL_PI_CREDENTIAL` when using a different credential reference, and edit
the local Pi template for its endpoint and model. Any OpenAI-compatible Chat Completions service can be
used; Venice is only the shipped example. Return to the deterministic worker with
`ARTIDEZI_LOCAL_HARNESS=fake just services-start`.

The **demo agent is deterministic**: it records the task document in `notes/` and proposes that
Markdown change. It does not interpret your instructions. Choose `demo-needs-input` to see a
question for the human, or `demo-failing-check` to see verification block publication. This keeps
UI and lifecycle iteration repeatable and free of model charges. The default worker is scripted; Pi above is opt-in. A separately configured worker needs its own model authentication and an
explicitly chosen execution boundary.

```sh
just services-test          # Create → revise → same MR → merge → done, plus failure paths
just services-browser-test  # Optional: Node 22+ and Chromium; desktop/mobile screenshots
just services-status
just services-tail-logs
just services-stop          # Preserve tasks, repositories and local credentials
```

Browser checks use Chromium's debugging protocol directly, with no npm dependencies, and save
screenshots under `var/screenshots/`. Set `CHROMIUM=/path/to/chromium` if needed. The test starts
an isolated headless browser with its sandbox disabled, visits only the loopback demo, and shuts
it down afterward. Both test commands create demo tasks you can explore in the UI. Browser checks
cover task creation/revision, failed checks, search/counters, stale search responses, draft and
attempt selection and draft preservation, keyboard focus, a 1rem (16px) text minimum and widths from 320px to desktop.

To iterate, edit the Rust UI or its CSS/JavaScript under `crates/server/src/ui/`, then run
`just services-start` again. It rebuilds the image and preserves data. The source Compose/config
files are in `etc/services/`; startup copies the configuration into `var/services/config/`.
Change those source templates for persistent local configuration changes. Preparation adds a
`local-admin` declaration with a generated initial password hash to the private copied config;
the SQLite password can change independently.

For additional private fixture projects or an external isolated worker, put a complete orchestrator
configuration in ignored `var/services/orchestrator.local.yaml`. `just services-start` uses it instead
of the default template and preserves it across rebuilds. If the override defines its own
`ui_access`, it must declare an enabled administrator;
otherwise preparation adds `local-admin`. Keep any initial password hashes in this ignored private
file; put service tokens under `var/services/secrets/`, and model credentials on the isolated worker.
Removing the override restores the default configuration on the next start. The shipped
workflow grants a real coding worker no workstation Docker socket or source directories.
A future Docker-capable local worker can use a deliberately weaker development boundary, but that
is a separate design choice rather than an isolation guarantee supplied by Compose.

Generated credentials and application data are ignored under `var/services/`; Forgejo stores its
data in a named Docker volume. `just services-reset` asks for confirmation before deleting both.
The local template uses `${ARTIDEZI_FORGEJO_PORT}` in the forge’s `web_base_url`; preparation
substitutes the configured port so browser links use the local Forgejo hostname. Internal API/Git
connections still use the Compose service name. CLI/lifecycle checks use loopback IPs without DNS;
the browser test pins the friendly hostnames to loopback through Chromium’s resolver rules.

Never commit generated credentials. To use different loopback ports, set `ARTIDEZI_UI_PORT` and
`ARTIDEZI_FORGEJO_PORT` when running `just services-start`; they are saved for later starts.

## Build and check

See [Releases](releases.md) for application versioning, release preparation and publication.

```sh
just build-dev
just check
just forgejo-test
just image artidezi:dev
just extract-binary artidezi:dev target/artidezi-linux-amd64
```

`just check` runs formatting, Clippy, the workspace's Rust tests and REUSE licensing. Strict Clippy
can be requested with `cargo clippy --workspace --all-targets -- -D warnings`. Offline Cargo flags
are usable when dependencies are cached. HTTP tests require permission to bind loopback ports.
The HTTP responsiveness tests gate local repository preparation before its real fetch and verify
that UI/authentication, worker leases, heartbeats and artifact transfers finish while it is blocked.
They also exercise concurrent scans without duplicate source claims and subsequent cancellation.
Store tests cover concurrent leasing/pause decisions and identical artifact uploads.

The default image contains a static musl binary, CA certificates and Git on Ubuntu LTS. Its build checks
that the binary needs no ELF interpreter or shared libraries. The Compose `development` stage adds Python
for the scripted worker and installs the pinned Pi coding agent. A standalone worker still needs its
selected harness and verification tools.
UI assets are embedded in the executable; serving the UI does not require Node or an asset directory.
The logo and favicon share the SVG in [branding assets](../../assets/branding/README.md).
Their URLs contain the SHA-256 of their embedded content. Matching assets use one-year immutable
public caching; HTML, errors, redirects and unversioned assets remain `no-store`. Valid older hash
URLs redirect to the current asset without caching the old address. Unknown asset names return 404.
Node/Chromium are optional test-time dependencies, not runtime dependencies.

## Select the appropriate test layer

| Layer | What it proves | Entry point |
| --- | --- | --- |
| Rust unit/integration | Contract, config, state, Git and controller transitions | `just check` |
| Real Forgejo contract | Actual API and Git behavior, including PR refs | `just forgejo-test` |
| Running local lifecycle | Authenticated UI/API through scripted worker and private forge | `just services-test` |
| Browser | Login, forms, polling/draft preservation and desktop/mobile layout | `just services-browser-test` |
| Scoped real-model experiment | Reasoning/patch behavior on frozen fixture inputs | Separately configured worker/project, explicit experiment scope |

Some Forgejo Rust tests are opt-in and return early without their integration environment. A green
ordinary Cargo run does not establish that Forgejo was contacted; the dedicated command provisions
and exercises a disposable instance. Scripted workers prove protocol/lifecycle behavior, not model
judgment. Real-model runs cost quota and need scoped credentials; passing one is not a substitute
for deterministic regressions.

GitHub tests in `just check` use disposable RSA keys and a loopback App/API fixture, plus local Git
repositories for the full discovery-to-result flow. They exercise token scope/renewal, refused writes,
source freshness, restart deduplication and persisted rate-limit backoff without GitHub access. The
forge crate's `test-fixtures` feature is only for those tests; ordinary release builds exclude it.
Use the read-only `github-check` preflight to verify an operator's actual installation separately.

Use a local fixture for source updates, force-push/closure, failed checks, human holds and publication
conflicts. Inspect the actual candidate and relevant negative cases, not just the agent summary.
Preserve source head, target bases, artifacts and test outcomes for a reproducible experiment. Keep
private experiment records under ignored state or the operator's private notes, not public docs.

The browser test selects `playground` explicitly, so extra projects in a local override do not change
its task scope. It also passively checks completed shadow tasks when present; it does not resume them.
Both lifecycle/browser scripts intentionally create playground history. Run them against the local
stack, not a live instance. Extra Codex projects can wait indefinitely without a connected real worker.

## UI conventions

Use direct labels and short copy. Principal cards use a shared neutral blue/slate `card-header` band
and larger titles than their subsections. Keep exact attempt navigation in artifact breadcrumbs.
Put task instructions, results and feedback in the main column;
put task settings in a compact top strip. Keep execution metadata beneath the attempt heading in
that attempt's card. Separate patches, agent logs and project checks; place each check's command,
status and output together, and explain that the project defines it and the worker runs it.
Keep content visible: use ordinary navigation for older attempts and full evidence, not accordions. Use the
available page width, including on large monitors; keep narrow search forms right-aligned. Use at least 1rem (16px at the default root size) for
all text, including secondary labels, code and mobile layouts. Hover may change colors or borders, but should not move elements. Pair status colors
with text or icons, and preserve visible keyboard focus. All pages share a header: root pages put their title/actions there, while inner pages use linked
breadcrumbs. Avoid duplicating root headings below it. A separate horizontal top navbar holds
main navigation, the brand and one shared desktop/mobile sign-out control; wrap whole navigation
items on small screens. Known forge links use a leading forge icon and a trailing external-link icon.
Keep automatic-task origin visible, and link detail pages to the exact incoming source. Show
evaluation mode on each incoming row; do not imply a global setting or completed source merge. New-task
forms use full-width cards and policy-limited delivery radio buttons. Textareas start compact and
grow with their content, including restored drafts.

The UI is server-rendered with embedded CSS/JavaScript. Search uses ordinary GET forms; JavaScript
adds debounced updates and ignores stale responses. Polling updates result regions separately from
instructions and feedback, preserving drafts and the selected attempt. Long instructions use a
large inline expansion button; serve the complete safe Markdown and show it by default without
JavaScript. The enhanced preview and expansion state remain outside polling replacements. Keep
old full-instructions links working through a redirect to the expanded content. Exercise these interactions
in Chromium when changing the layout, including wide screens, visible activity and both
confirmation outcomes. Rust HTTP checks alone cannot catch lost browser state. Session changes
also need a restart check:
an existing login and its CSRF token should survive with unchanged configuration, while logout,
expiry and password rotation must invalidate it.

Task instructions render as Markdown with raw HTML escaped and only explicit HTTP(S)/email links.
Images become links without automatic fetches. The patch viewer reads existing, verified text
artifacts through the same authenticated task/attempt checks as raw logs. It never applies patches
or executes repository code. Preview bounds are 512 KiB, 4,000 lines and 8,000 characters per line;
the original remains available through the raw link (subject to the existing 8 MiB text-artifact limit).

## Maintain documentation with the code

- User behavior/configuration changes belong in the relevant [user guide](../user-guide/README.md).
- Boundary/state/contract changes belong in [architecture](../architecture/README.md), with source links.
- Build/test/local-stack changes belong here. Keep the README short and update its navigation.
- Label planned features explicitly. Do not turn a schema enum or private experiment into a support claim.
- Use generic examples across libraries, applications and deployment code; keep the design useful
  beyond any one operator's ecosystem without promising a general automation framework.
- Keep deployment hosts, private inventories, credentials and operator-specific history outside the codebase.
- Give new files SPDX/REUSE coverage. For docs-only changes, run `reuse lint`, `git diff --check` and
  verify relative Markdown links/anchors; rebuilding the runtime or rerunning model tasks is unnecessary.

Do not duplicate Rust structs or a full API schema in prose. Link to types and schema-export commands,
and explain the invariants and operational consequences that the types alone do not convey.
