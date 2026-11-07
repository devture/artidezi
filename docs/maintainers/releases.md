<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Releases

Releases are automatic. Nobody picks a version or pushes a tag.

## Versions

A release is a CalVer Git tag, `vYYYY.MM.DD.N`: the date is in UTC and `N` counts from `0` within the day.
Cargo packages stay at `0.0.0` with publishing disabled.
Release builds embed the real version through `ARTIDEZI_VERSION`, which the CLI, the UI footer, worker supervisor records and the forge User-Agent all report.
Every other build reports `dev`.
Document schema and prompt versions have their own compatibility rules and don't follow the application version.

## When a release happens

After CI on `main`, and every 30 minutes, [`release/plan-release.py`](../../release/plan-release.py) releases `main` if:

- the image changed since the last published GitHub release, so a run which failed after tagging gets replaced by the next one;
- CI passed for that commit;
- `main` hasn't moved for 30 minutes;
- no Renovate update to the image is about to land. This wait ends once the head is 90 minutes old.

The image's paths are the `COPY` sources of the stages that the last stage builds on, plus those stages themselves and `.dockerignore`. So neither documentation nor a change to the `development` stage, like a Pi bump, causes a release.
Keep each `COPY` on 1 line without JSON syntax, or the release scripts refuse it.

For a release sooner, start the workflow by hand with `gh workflow run release.yml`.
It skips the waiting, but still needs an image change and passing CI.

## What a release publishes

The `Release` workflow runs the full checks with the new version embedded, tags the tested commit and publishes the exact images those checks built:

| Output | Where |
| --- | --- |
| Multi-architecture image | `ghcr.io/devture/artidezi:vYYYY.MM.DD.N` and `latest` |
| Single-architecture images | `ghcr.io/devture/artidezi:vYYYY.MM.DD.N-amd64` and `-arm64` |
| Static worker binaries | GitHub Release assets `artidezi-linux-amd64`, `artidezi-linux-arm64` and `SHA256SUMS` |
| Release notes | The GitHub Release |

A commit on `main` that passes CI is also published as `edge` and `edge-<full commit hash>`, unless a newer commit has already replaced it. These aren't releases.

Images and binaries carry build provenance attestations:

```sh
gh attestation verify oci://ghcr.io/devture/artidezi:<tag> --owner devture
gh attestation verify artidezi-linux-amd64 --owner devture
```

## Release notes

[`release/release-notes.py`](../../release/release-notes.py) writes the notes from the commits since the previous release.
Each non-merge commit that touches the image is listed by its subject, so write subjects that read well to people who run ArtiDezi.

Optional trailers improve the notes. Git only reads trailers in the message's last paragraph, so keep them in 1 block:

- `Release-note: <text>` rewords the entry, or includes a commit outside the image, such as documentation.
- `Release-note: skip` leaves out a commit nobody upgrading needs to know about.
- `Breaking-change: <what breaks and what to do>` lists the commit first, under "Breaking changes".

## Upgrading a deployment

Back up the orchestrator's database and configuration first.
Run the same release on orchestrators and workers, and keep the installed harness versions within the orchestrator's profile constraints.

## Building by hand

A registry is optional. To build an image, extract the same binary for workers and copy the image to a host over SSH:

```sh
just image artidezi:custom 2026.11.07.0
just extract-binary artidezi:custom target/artidezi-custom
just deploy-image root@controller.example.org artidezi:custom
```

The second argument of `just image` is the version to embed; it defaults to `dev`.
