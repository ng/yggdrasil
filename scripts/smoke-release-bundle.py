#!/usr/bin/env python3
"""Verify a locally built bundle and run its binary in a disposable profile.

Offline smoke denies curl, initializes/migrates, checks persistent identity,
reuses the installation without an archive, and stops the owned supervisor.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import time

ROOT = Path(__file__).resolve().parents[1]


def verified_files(bundle):
    files = {}
    total = 0
    with tarfile.open(bundle) as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            if (not member.isfile() or path.is_absolute() or ".." in path.parts
                    or str(path) != member.name or member.name in files
                    or len(files) >= 32 or not 0 <= member.size <= 128 * 1024 * 1024
                    or member.mode not in (0o644, 0o755)):
                raise ValueError("unsafe bundle member")
            total += member.size
            if total > 192 * 1024 * 1024:
                raise ValueError("bundle exceeds expanded limit")
            files[member.name] = (archive.extractfile(member).read(), member.mode)
    manifest = json.loads(files["manifest.json"][0])
    if manifest["schema"] != 1 or manifest["flavor"] not in ("online", "offline"):
        raise ValueError("unsupported bundle manifest")
    if manifest["postgres_release"] != json.loads((ROOT / "src/db/packages.json").read_text()):
        raise ValueError("bundle PostgreSQL release is not the source pin")
    if set(files) != set(manifest["files"]) | {"manifest.json"}:
        raise ValueError("bundle members differ from manifest")
    for name, expected in manifest["files"].items():
        data, mode = files[name]
        if (len(data) != expected["bytes"] or hashlib.sha256(data).hexdigest() != expected["sha256"]
                or mode != expected["mode"] or mode not in (0o644, 0o755)):
            raise ValueError(f"bundle member differs from manifest: {name}")
    return manifest, files


def smoke(bundle, directory, allow_download=False):
    manifest, files = verified_files(bundle)
    directory.mkdir(mode=0o700)
    extracted = directory / "package with spaces"
    extracted.mkdir()
    for name, (data, mode) in files.items():
        path = extracted / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)
    binary = extracted / "bin/ygg"
    env = {k: v for k, v in os.environ.items() if not k.startswith("YGG_") and k != "DATABASE_URL"}
    env.update(YGG_DB_MODE="managed", YGG_CONFIG_DIR=str(directory / "config"),
               YGG_DATA_DIR=str(directory / "data"))
    blocker = directory / "deny-download"
    blocker.mkdir()
    (blocker / "curl").write_text('#!/bin/sh\necho "unexpected download during bundle smoke" >&2\nexit 86\n')
    (blocker / "curl").chmod(0o755)
    if not allow_download or manifest["flavor"] == "offline":
        env["PATH"] = str(blocker) + os.pathsep + env["PATH"]

    def run(*args, timeout=90):
        return subprocess.check_output(args, env=env, cwd=directory, stdin=subprocess.DEVNULL,
                                       stderr=subprocess.STDOUT, timeout=timeout, text=True)

    timings = {}

    def measured(name, *args):
        started = time.perf_counter()
        result = run(*args)
        timings[name] = (time.perf_counter() - started) * 1000
        return result

    version = run(str(binary), "--version", timeout=30).strip()
    if version != f'ygg {manifest["version"]}':
        raise ValueError("packaged binary version mismatch")
    if manifest["flavor"] == "online" and not allow_download:
        print(json.dumps({"flavor": "online", "version": version, "verified": True,
                          "download_init_tested": False}))
        return
    try:
        measured("fresh_profile_init", str(extracted / "initialize"), "--yes", "--skip", "tmux,jq,rtk,hooks,project")
        first = json.loads(run(str(binary), "db", "status", "--json"))
        if first["mode"] != "managed" or not first.get("cluster_id") or not first.get("supervisor_pid") or first["postgres"]["state"] != "ready":
            raise ValueError("packaged init did not leave a supervised managed cluster")
        # Ordinary init must reuse installed binaries without the shipped archive.
        if (extracted / "postgres").exists():
            for path in (extracted / "postgres").iterdir():
                path.unlink()
        env["PATH"] = str(blocker) + os.pathsep + env["PATH"]
        measured("running_cluster_init_reuse", str(binary), "init", "--yes", "--skip", "tmux,jq,rtk,hooks,project")
        second = json.loads(run(str(binary), "db", "status", "--json"))
        if first != second:
            raise ValueError("repeat init changed cluster or process identity")
        run(str(binary), "db", "stop", "--timeout", "30", "--json", timeout=40)
        measured("stopped_cluster_start", str(binary), "db", "start", "--timeout", "30", "--json")
        restarted = json.loads(run(str(binary), "db", "status", "--json"))
        if (restarted["cluster_id"] != first["cluster_id"]
                or restarted["postgres"]["state"] != "ready"
                or not restarted.get("supervisor_pid")):
            raise ValueError("restart changed cluster identity or failed readiness")
        # Real backup exercises packaged pg_dump, runtime/owner routing and schema.
        run(str(binary), "db", "backup", str(directory / "backup"), "--json")
        run(str(binary), "db", "verify-backup", str(directory / "backup"), "--json")
    finally:
        # The environment and cwd above identify only this disposable profile.
        if (directory / "data/postgres").exists():
            run(str(binary), "db", "stop", "--timeout", "30", "--json", timeout=40)
    # Emit success only after teardown succeeds; never certify a partial run.
    print(json.dumps({"flavor": manifest["flavor"], "version": version, "verified": True,
                      "cluster_id": first["cluster_id"], "backup_verified": True,
                      "target": manifest["target"],
                      "build_source_commit": manifest["build_source_commit"],
                      "timing_ms": timings,
                      "timing_scope": "Single wall-clock lifecycle observations, including CLI startup; fresh profile and stopped initialized cluster, not OS cache eviction or a cold machine. Online init includes download. Separate from warm hook latency; no percentile or threshold claim."}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--allow-download", action="store_true", help="initialize the online bundle using its pinned download")
    args = parser.parse_args()
    try:
        smoke(args.bundle.absolute(), args.directory.absolute(), args.allow_download)
    except subprocess.CalledProcessError as error:
        print(error.output or "packaged command failed without output", flush=True)
        raise
