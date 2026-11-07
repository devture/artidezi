<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Multi-user web access

The optional UI uses named local accounts stored in the instance SQLite database. Accounts have
stable IDs, display names, an optional instance-administrator flag, and exact-project or `*`
grants. Versioned `ui_access` configuration owns names, enabled state, administrator status and
grants. Current passwords live in SQLite and can drift from initial configured hashes. The
[operator guide](../user-guide/operations.md#enabling-the-web-ui) shows provisioning and
self-service password changes. Each account's time zone also lives in SQLite: new declarations
start at UTC and may adopt the browser zone on first signed-in visit; migrated accounts stay at
UTC until edited. Users can change their own zone on **Account** without revoking sessions or
changing config-owned access. SSO and personal API tokens remain future work.

## Project grants

| Access | Permitted work |
| --- | --- |
| Read | Project summaries, tasks, incoming changes, results, activity and evidence in that project |
| Write | Read access plus task creation, revision, closing, publication retry and project discovery controls |

For example, `application: write` plus `*: read` allows maintenance work in `application` and
viewing other projects. Grants are additive: the highest matching access wins, independent of
order. No matching grant means no project access. A narrower read grant cannot revoke a broad
write grant. An instance administrator can read and write all projects; project write access does
not grant instance administration. The admin API bearer remains a separate instance-wide credential.

Project read access covers the whole task, including configured context repositories, logs and
patches. An operator must account for that when composing projects. These grants do not turn one
shared orchestrator, worker pool and credential store into isolated tenants. User rights also
cannot expand a project's configured repositories, delivery modes or forge effects.

## Enforcement and attribution

The server resolves the user from an opaque SQLite-backed session. Only a session's stored user ID
selects its current database grants; no browser actor field is trusted. Lists, search and counters are
filtered before rendering. Exact task, event, log and patch URLs check project access before
loading related data. Mutations check write access inside the serialized controller operation.
The authenticated session also loads project-filtered Tasks and Incoming changes navigation counts;
the shared page shell renders those counts on every signed-in page. Counts refresh on the next
request rather than relying on each page controller to supply them.
The global discovery scan requires an instance administrator; project writers can scan a selected
project. Worker views show only attempts and pools used by visible projects.

Browser actions use `user:<id> (<display name at the time>)` as their durable actor string.
The stable ID and display snapshot remain in that string if a user is renamed or removed later.
Caller-supplied admin API actor strings are labels, not evidence of an individual person. Automated controller and worker actors remain separate.

Accounts and grants are read from SQLite on every browser request. Disabling an account or
changing grants takes effect on the next request. Changing or resetting a password revokes that
account's sessions; a new login is required. The account page provides a current-password-confirmed
password change. Account creation and access changes are made in config and applied at UI startup;
the local CLI can inspect accounts and reset passwords for recovery.
A durable flag shows an initial/reset-password notice throughout the signed-in UI until the user
changes their password. It is cleared by that change, not by comparing hash strings to config.
Configured account identity and access are reconciled at UI startup in one database transaction.
Existing current passwords and password-change flags are preserved. A declaration change updates
name, administrator status, enabled state and grants; omission disables the account, clears grants
and revokes its sessions. At least one enabled administrator is required. Changing a configured
initial hash does not reset an existing password. The config must be kept private because its
initial hashes are authentication material. The browser has no account or role editor.

An already accepted task keeps its frozen project policy and can continue publication after the
initiating user's access is removed; revocation stops new human actions.

## Future SSO and user API

OIDC can later authenticate users while ArtiDezi keeps this project grant model. Bind an external
identity by issuer and subject, not email or display name. Use an OIDC client with server-side
authorization-code exchange, PKCE, state and nonce, and validate the issuer, signature, audience
and expiry. Provider tokens should stay on the server. Keycloak is one possible provider; its
[OIDC guide](https://www.keycloak.org/securing-apps/oidc-layers) describes the flow. Explicit
group mapping would need allowlisted claim values and a documented reauthentication interval:
login-time group claims alone cannot immediately revoke a browser session.

Personal API tokens should use the same principal and project checks on separate user API routes.
The `/admin/v1` and `/worker/v1` bearers keep their distinct authority. Neither OIDC nor PAT routes
are implemented.
