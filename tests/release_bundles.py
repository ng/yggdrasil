#!/usr/bin/env python3
"""Artifact integrity and publication regressions; native lifecycle smoke is separate."""
import importlib.util
import json
from pathlib import Path
import tempfile
import struct
import sys

sys.dont_write_bytecode = True
import unittest
from unittest.mock import patch
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


build = module("bundle_build", "build-release-bundles.py")
smoke = module("bundle_smoke", "smoke-release-bundle.py")


class MacDistribution(unittest.TestCase):
    def test_distribution_checks_fail_closed(self):
        audit = module("mac_audit", "audit-macos-release-bundle.py")
        image = struct.pack("<IIII", 0xFEEDFACF, 0x100000C, 0, 2)
        signed = "Authority=Developer ID Application: Fixture\nTeamIdentifier=FIXTURE\nCodeDirectory flags=0x10000(runtime)\n"
        for metadata, failed_check, expected in [
                (signed, None, True), (signed, "--verify", False),
                (signed, "--assess", False), ("Signature=adhoc\n", None, False),
                (signed.replace("(runtime)", ""), None, False)]:
            with self.subTest(metadata=metadata, failed_check=failed_check):
                def run(args, **kwargs):
                    return SimpleNamespace(returncode=int(failed_check in args) if failed_check else 0,
                                           stdout="", stderr=metadata if "--display" in args else "")
                with tempfile.TemporaryDirectory() as tmp, patch.object(audit.subprocess, "run", run):
                    self.assertEqual(audit.inspect("ygg", image, Path(tmp))["passed"], expected)

    def test_unsupported_macho_is_not_silently_skipped(self):
        audit = module("mac_audit", "audit-macos-release-bundle.py")
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                audit.inspect("fat", b"\xca\xfe\xba\xbe" + bytes(20), Path(tmp))
            self.assertIsNone(audit.inspect("README", b"plain text", Path(tmp)))


class BundleTests(unittest.TestCase):
    def fixture(self, directory, mutate=None):
        files = {"bin/ygg": (b"fixture binary", 0o755), "README.txt": (b"notes", 0o644)}
        manifest = {"schema": 1, "flavor": "online",
                    "postgres_release": json.loads((ROOT / "src/db/packages.json").read_text()),
                    "files": {name: {"sha256": build.sha(data), "bytes": len(data), "mode": mode}
                              for name, (data, mode) in files.items()}}
        files["manifest.json"] = json.dumps(manifest).encode(), 0o644
        if mutate:
            mutate(files)
        path = directory / "bundle.tar.gz"
        path.write_bytes(build.tar_bytes(files, 42))
        return path

    def test_deterministic_archive_preserves_executable_mode(self):
        files = {"z": (b"z", 0o644), "bin/ygg": (b"binary", 0o755)}
        self.assertEqual(build.tar_bytes(files, 42), build.tar_bytes(dict(reversed(list(files.items()))), 42))
        with tempfile.TemporaryDirectory() as tmp:
            manifest, result = smoke.verified_files(self.fixture(Path(tmp)))
            self.assertEqual(result["bin/ygg"][1], 0o755)
            self.assertEqual(manifest["flavor"], "online")

    def test_changed_bytes_and_unlisted_files_fail(self):
        for change in [lambda f: f.update({"bin/ygg": (b"modified bytes", 0o755)}),
                       lambda f: f.update({"extra": (b"surprise", 0o644)}),
                       lambda f: f.update({"manifest.json": (f["manifest.json"][0], 0o4777)})]:
            with tempfile.TemporaryDirectory() as tmp:
                with self.assertRaises(ValueError):
                    smoke.verified_files(self.fixture(Path(tmp), change))

    def test_unsafe_paths_fail_before_extraction(self):
        for name in ["../escape", "/absolute", "a/../escape"]:
            with tempfile.TemporaryDirectory() as tmp:
                with self.assertRaises(ValueError):
                    smoke.verified_files(self.fixture(Path(tmp), lambda f: f.update({name: (b"bad", 0o644)})))
                self.assertEqual([p.name for p in Path(tmp).iterdir()], ["bundle.tar.gz"])

    def test_publication_never_replaces_existing_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "artifact"
            build.publish(path, b"first")
            with self.assertRaises(FileExistsError):
                build.publish(path, b"second")
            self.assertEqual(path.read_bytes(), b"first")
            self.assertEqual([p.name for p in Path(tmp).iterdir()], ["artifact"])

    def test_wrong_binary_architecture_is_rejected(self):
        arm = struct.pack("<II", 0xFEEDFACF, 0x100000C)
        build.verify_binary_target(arm, "aarch64-apple-darwin")
        with self.assertRaises(ValueError):
            build.verify_binary_target(arm, "x86_64-apple-darwin")
        with self.assertRaises(ValueError):
            build.verify_binary_target(b"#!/bin/sh", "x86_64-unknown-linux-gnu")

    def test_wrong_postgres_bytes_and_symlink_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "archive"
            path.write_bytes(b"wrong release")
            with self.assertRaisesRegex(ValueError, "pinned manifest"):
                build.pinned_inputs(path, "aarch64-apple-darwin")
            link = Path(tmp) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                build.read_regular(link, 100)


if __name__ == "__main__":
    unittest.main()
