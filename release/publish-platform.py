#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
#
# SPDX-License-Identifier: Apache-2.0
"""Publish a tested single-platform image archive under its tag, or reuse an identical earlier push.

The archive (from `docker image save`) is pushed with skopeo rather than through the local Docker
image store, so the published manifest has the same form whichever store the runner uses.
A tag that already holds a different image is never overwritten.
"""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tempfile

DIGEST = r"sha256:[0-9a-f]{64}"


def skopeo(*args):
    return subprocess.run(["skopeo", *args], capture_output=True, check=False)


def checked(*args):
    result = skopeo(*args)
    if result.returncode:
        sys.exit(f"skopeo {' '.join(args)} failed:\n{result.stderr.decode()}")
    return result.stdout


def config_digest(raw_manifest, where):
    """The image identity: its config blob digest, which is the same however the layers are compressed."""
    digest = json.loads(raw_manifest).get("config", {}).get("digest", "")
    if not re.fullmatch(DIGEST, digest):
        sys.exit(f"{where} is not a single-platform image manifest")
    return digest


def manifest_digest(raw_manifest):
    return "sha256:" + hashlib.sha256(raw_manifest).hexdigest()


def publish(archive, repository, tag, arch):
    source = f"docker-archive:{archive}"
    target = f"docker://{repository}:{tag}"
    config = json.loads(checked("inspect", "--config", source))
    platform = f"{config.get('os')}/{config.get('architecture')}"
    if platform != f"linux/{arch}":
        sys.exit(f"The archive holds a {platform} image, not linux/{arch}")
    tested = config_digest(checked("inspect", "--raw", source), "The archive")

    existing = skopeo("inspect", "--raw", target)
    if existing.returncode == 0:
        published = config_digest(existing.stdout, f"{repository}:{tag}")
        if published != tested:
            sys.exit(f"{repository}:{tag} already holds a different image (config {published}, tested {tested}); "
                     "refusing to overwrite it")
        digest = manifest_digest(existing.stdout)
        print(f"Reusing {repository}:{tag}, which already holds the tested image")
    else:
        error = existing.stderr.decode()
        # Network and authentication errors must fail closed rather than overwrite tags.
        if not re.search(r"manifest unknown|not found", error):
            sys.exit(f"Cannot tell whether {repository}:{tag} exists:\n{error}")
        with tempfile.TemporaryDirectory() as directory:
            digest_file = pathlib.Path(directory) / "digest"
            checked("copy", "--retry-times", "3", "--format", "v2s2", "--digestfile", str(digest_file),
                    source, target)
            digest = digest_file.read_text().strip()
        pushed = checked("inspect", "--raw", f"docker://{repository}@{digest}")
        if manifest_digest(pushed) != digest or config_digest(pushed, f"{repository}@{digest}") != tested:
            sys.exit(f"{repository}@{digest} does not hold the tested image")
        print(f"Published {repository}:{tag}")
    if not re.fullmatch(DIGEST, digest):
        sys.exit(f"Unexpected manifest digest {digest!r}")
    return f"{repository}@{digest}"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--archive", required=True, help="image archive from docker image save")
    parser.add_argument("--repository", required=True, help="e.g. ghcr.io/devture/artidezi")
    parser.add_argument("--tag", required=True, help="the single-platform tag, e.g. v2026.11.10.0-amd64")
    parser.add_argument("--arch", required=True, choices=["amd64", "arm64"])
    parser.add_argument("--digest-file", required=True, type=pathlib.Path,
                        help="receives repository@digest of the published manifest")
    args = parser.parse_args()
    reference = publish(args.archive, args.repository, args.tag, args.arch)
    args.digest_file.write_text(reference + "\n")
    print(reference)
