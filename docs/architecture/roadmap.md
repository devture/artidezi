<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Future directions

This is a bounded design backlog, not implemented behavior or permission to publish/deploy anything.
The [current architecture](README.md) and user guide describe what works today.

## Next useful increments

1. Keep proving source assessment with local Forgejo fixtures and real repository copies: unchanged
   update, adaptation, source movement, holds, failures and coordinated repository work.
2. Exercise the implemented COMMENT reviews and human-reviewed adaptation PRs on enrolled pilots.
   Preserve source/head linkage, freshness, branch ownership and retry reconciliation.
   Broader automatic delivery needs independently verified evidence. Safe source approval/merge
   is available through explicit policy; adaptation PRs still require review.
3. Reduce repeated repository metadata where actual multi-consumer configurations need it. The
   [project-composition proposal](project-composition.md) separates shared definitions from explicit
   project access. Prove a real upstream release followed by independent consumer bump/adaptation
   tasks; simultaneous candidate PRs do not prove that chain. Replace updater bypass only for the
   selected migrated dependencies once the replacement workflow works. No recursive project graph
   is needed.
4. Enroll selected GitHub repositories through the implemented App-authenticated dry-run adapter;
   follow the [enrollment guide](../user-guide/github-enrollment.md). Live installation preflight and
   independently reviewed pilot outcomes precede enabling publication. Local tests do not
   validate an operator's App setup or turn visible CI into automatic merge authorization.

## Later, when actual use needs them

- Independent source/candidate verification and richer required-check policy beyond the implemented
  opt-in approval/merge gates.
- A small PR reviewer that posts/updates a concise owned comment, without push/merge authority.
- Worker Docker ergonomics, cleanup/reset and backup/restore procedures.
- Optional OIDC login and explicit group-to-project grant mapping on top of the implemented
  [named-user web access](web-access.md). Personal API tokens would need separate scoped routes.
  This is not the same as hosting multiple trust domains together.
- Chat task submission and thread-based feedback. Natural-language routing would resolve to existing
  explicit projects/policies and authenticated users; it must not infer new authority from chat text.

Automatic issue intake, arbitrary event predicates, recursive related-project activation, an action
DAG, generic plugin/rules engines and bot conversation parsing are not prerequisites for maintenance
PR handling. Prefer the existing web UI and explicit tasks until repeated use justifies more.
Deployment-specific hosting/migration choices and private integration ideas belong in private planning,
not in public source documentation or example credentials.
