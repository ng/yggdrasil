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
    parser.add_argument("--upgrade-source", action="store_true", help="prepare pinned 16.14 upgrade fixture")
    parser.add_argument("--major", type=int, choices=[16, 18], default=16)
    args = parser.parse_args()
    if args.upgrade_source and args.major != 16:
        parser.error("--upgrade-source is a PostgreSQL 16 fixture")
    if args.major == 18:
        manifest = json.loads((Path(__file__).resolve().parents[1] / "src/db/packages-18.json").read_text())
    package = next(p for p in manifest["packages"] if p["target"] == args.target)
    if args.upgrade_source:
        # Test-only previous patch; never changes the product installation pin.
        previous = {
            "aarch64-apple-darwin": (12101670, "a7a4846456df26d27f815267dfe725b4ad4f46312c032e7b5939468250a4891c"),
            "x86_64-apple-darwin": (12448094, "c5ecdea2528e29503140e259c043002f6f8f2e9d1ee2f1decb44e8b394254820"),
            "x86_64-unknown-linux-gnu": (11068225, "3541705c9ca2fdbb0bfc7562b10a36aac155f18d45577d8ec6725b874ccada4a"),
        }
        size, digest = previous[args.target]
        root = f"postgresql-16.14.0-{args.target}"
        package = {
            "root": root, "bytes": size, "sha256": digest,
            "url": f"https://github.com/theseus-rs/postgresql-binaries/releases/download/16.14.0/{root}.tar.gz",
        }
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
    if args.major == 18:
        env = f"YGG_TEST_PG18_ARCHIVE={archive}\nYGG_TEST_PG18_BIN={binary}\n"
    elif args.upgrade_source:
        env = f"YGG_TEST_PG_OLD_BIN={binary}\n"
    if "GITHUB_ENV" in os.environ:
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as output:
            output.write(env)
    print(env, end="")


if __name__ == "__main__":
    main()
