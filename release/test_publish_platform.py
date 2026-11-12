# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
#
# SPDX-License-Identifier: Apache-2.0

import hashlib
import importlib.util
import json
import pathlib
import subprocess
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("publish", pathlib.Path(__file__).with_name("publish-platform.py"))
publish = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publish)

REPOSITORY = "registry.example.invalid/project/image"
TESTED = "sha256:" + "a" * 64
OTHER = "sha256:" + "b" * 64


def manifest(config_digest):
    return json.dumps({"schemaVersion": 2, "config": {"digest": config_digest}, "layers": []}).encode()


def digest_of(raw):
    return "sha256:" + hashlib.sha256(raw).hexdigest()


class FakeSkopeo:
    """Answers skopeo calls from an in-memory registry holding at most one tag."""

    def __init__(self, existing=None, missing_error="manifest unknown", pushed=TESTED, arch="amd64"):
        self.existing, self.missing_error, self.pushed, self.arch = existing, missing_error, pushed, arch
        self.copied = False

    def __call__(self, *args):
        def result(code=0, stdout=b"", stderr=b""):
            return subprocess.CompletedProcess(args, code, stdout, stderr)

        command, *rest = args
        reference = rest[-1]
        if command == "inspect" and rest[0] == "--config":
            return result(stdout=json.dumps({"os": "linux", "architecture": self.arch}).encode())
        if command == "inspect" and reference.startswith("docker-archive:"):
            return result(stdout=manifest(TESTED))
        if command == "inspect" and "@" in reference:
            return result(stdout=manifest(self.pushed))
        if command == "inspect":
            if self.existing is None:
                return result(1, stderr=self.missing_error.encode())
            return result(stdout=self.existing)
        if command == "copy":
            self.copied = True
            digest_file = pathlib.Path(rest[rest.index("--digestfile") + 1])
            digest_file.write_text(digest_of(manifest(self.pushed)))
            return result()
        raise AssertionError(f"unexpected skopeo call {args}")


class PublishPlatform(unittest.TestCase):
    def run_publish(self, fake, arch="amd64"):
        with mock.patch.object(publish, "skopeo", fake):
            return publish.publish("image.tar", REPOSITORY, "v1-amd64", arch)

    def test_pushes_a_new_tag_and_returns_the_pushed_digest(self):
        fake = FakeSkopeo()
        self.assertEqual(self.run_publish(fake), f"{REPOSITORY}@{digest_of(manifest(TESTED))}")
        self.assertTrue(fake.copied)

    def test_reuses_a_tag_that_already_holds_the_tested_image(self):
        existing = manifest(TESTED)
        fake = FakeSkopeo(existing=existing)
        self.assertEqual(self.run_publish(fake), f"{REPOSITORY}@{digest_of(existing)}")
        self.assertFalse(fake.copied)

    def test_refuses_to_overwrite_a_different_image(self):
        fake = FakeSkopeo(existing=manifest(OTHER))
        with self.assertRaises(SystemExit):
            self.run_publish(fake)
        self.assertFalse(fake.copied)

    def test_refuses_an_index_under_the_tag(self):
        fake = FakeSkopeo(existing=json.dumps({"schemaVersion": 2, "manifests": []}).encode())
        with self.assertRaises(SystemExit):
            self.run_publish(fake)
        self.assertFalse(fake.copied)

    def test_fails_closed_when_the_tag_cannot_be_read(self):
        fake = FakeSkopeo(missing_error="unauthorized: authentication required")
        with self.assertRaises(SystemExit):
            self.run_publish(fake)
        self.assertFalse(fake.copied)

    def test_rejects_an_archive_for_another_architecture(self):
        fake = FakeSkopeo(arch="arm64")
        with self.assertRaises(SystemExit):
            self.run_publish(fake)
        self.assertFalse(fake.copied)

    def test_rejects_a_push_that_does_not_hold_the_tested_image(self):
        with self.assertRaises(SystemExit):
            self.run_publish(FakeSkopeo(pushed=OTHER))


if __name__ == "__main__":
    unittest.main()
