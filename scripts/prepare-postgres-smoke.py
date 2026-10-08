#!/usr/bin/env python3
"""CI-only preparation of a checksum-pinned upstream release artifact.

Product installation is implemented in Rust; this helper does not provision
operator clusters and requires a fresh disposable destination.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile


def main():
    manifest = json.loads(
        (Path(__file__).resolve().parents[1] / "src/db/packages.json").read_text()
    )
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=[p["target"] for p in manifest["packages"]])
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    package = next(p for p in manifest["packages"] if p["target"] == args.target)
    assert 0 < package["bytes"] <= 64 * 1024 * 1024
    assert Path(package["root"]).name == package["root"]
    destination = args.directory.absolute()
    destination.mkdir(mode=0o700)
    archive = destination / (package["root"] + ".tar.gz")
    subprocess.run([
        "curl", "-q", "--fail", "--location", "--silent", "--show-error",
        "--proto", "=https", "--proto-redir", "=https", "--max-time", "180",
        "--max-filesize", str(package["bytes"]), "--output", str(archive), package["url"],
    ], check=True)
    data = archive.read_bytes()
    if len(data) != package["bytes"] or hashlib.sha256(data).hexdigest() != package["sha256"]:
        raise ValueError("PostgreSQL archive differs from pinned manifest")
    with tarfile.open(archive) as source:
        source.extractall(destination, filter="data")
    binary = destination / package["root"] / "bin"
    env = f"YGG_TEST_PG_ARCHIVE={archive}\nYGG_TEST_PG_BIN={binary}\nYGG_TEST_PG_MAJOR=16\n"
    if "GITHUB_ENV" in os.environ:
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as output:
            output.write(env)
    print(env, end="")


if __name__ == "__main__":
    main()
