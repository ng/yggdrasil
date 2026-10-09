#!/usr/bin/env python3
"""Build deterministic online/offline native bundles from a verified PG archive.

No downloads or cluster operations. CI supplies the freshly built native binary;
manifest hashes bind shipped bytes, not an independently reproducible build claim.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import stat
import struct
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError(f"not a regular file: {path}")
        data = source.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"file exceeds size limit: {path}")
    return data


def native_target():
    machine = {"arm64": "aarch64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
    suffix = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu"}.get(platform.system())
    return f"{machine}-{suffix}"


def verify_binary_target(data, target):
    if target.endswith("apple-darwin"):
        cpu = {"aarch64-apple-darwin": 0x100000C, "x86_64-apple-darwin": 0x1000007}[target]
        valid = len(data) >= 8 and struct.unpack_from("<II", data) == (0xFEEDFACF, cpu)
    else:
        valid = (len(data) >= 20 and data[:6] == b"\x7fELF\x02\x01"
                 and struct.unpack_from("<H", data, 18)[0] == 62)
    if not valid:
        raise ValueError("binary architecture differs from bundle target")


def pinned_inputs(archive, target):
    release = json.loads((ROOT / "src/db/packages.json").read_text())
    package = next((p for p in release["packages"] if p["target"] == target), None)
    if not package:
        raise ValueError("target absent from pinned PostgreSQL manifest")
    data = read_regular(archive, 64 * 1024 * 1024)
    if len(data) != package["bytes"] or sha(data) != package["sha256"]:
        raise ValueError("PostgreSQL archive differs from pinned manifest")
    notices = {}
    with tarfile.open(fileobj=io.BytesIO(data)) as source:
        for name in ("LICENSE", "COPYRIGHT"):
            member = source.getmember(f'{package["root"]}/{name}')
            if not member.isfile() or member.size > 1024 * 1024:
                raise ValueError("invalid PostgreSQL notice")
            notices[f"licenses/postgresql-{name}.txt"] = (source.extractfile(member).read(), 0o644)
    return release, package, data, notices


def tar_bytes(files, epoch):
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for name, (data, mode) in sorted(files.items()):
                info = tarfile.TarInfo(name)
                info.size, info.mode, info.mtime = len(data), mode, epoch
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def publish(path, data):
    # Hard-link publication refuses replacement even if another builder wins.
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".bundle-") as stage:
        stage.write(data)
        stage.flush()
        os.fsync(stage.fileno())
        os.chmod(stage.name, 0o644)
        os.link(stage.name, path)


def assemble(binary, archive, target, output):
    if target != native_target():
        raise ValueError("build and smoke-test bundles on their native target")
    release, package, pg_bytes, notices = pinned_inputs(archive, target)
    binary = binary.absolute()
    binary_bytes = read_regular(binary, 128 * 1024 * 1024)
    verify_binary_target(binary_bytes, target)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"[0-9A-Za-z.+-]+", version):
        raise ValueError("invalid release version")
    actual = subprocess.check_output([binary, "--version"], timeout=30, text=True).strip()
    if actual != f"ygg {version}":
        raise ValueError("binary version differs from Cargo.toml")
    if read_regular(binary, 128 * 1024 * 1024) != binary_bytes:
        raise ValueError("binary changed during verification")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    epoch = int(subprocess.check_output(["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT))
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT))
    output.mkdir(parents=True, exist_ok=False)
    checksums = {}
    # Keep the plain binary name compatible with the existing online installer.
    binary_name = f"ygg-{target}"
    publish(output / binary_name, binary_bytes)
    checksums[binary_name] = sha(binary_bytes)
    for flavor in ("online", "offline"):
        files = dict(notices)
        files["bin/ygg"] = binary_bytes, 0o755
        files["licenses/ygg-LICENSE.txt"] = (ROOT / "LICENSE").read_bytes(), 0o644
        files["licenses/openssl-LICENSE.txt"] = (ROOT / "src/db/licenses/openssl-3.6.3-LICENSE.txt").read_bytes(), 0o644
        files["postgres-packages.json"] = (ROOT / "src/db/packages.json").read_bytes(), 0o644
        pg_path = f'postgres/{package["root"]}.tar.gz'
        if flavor == "offline":
            files[pg_path] = pg_bytes, 0o644
        argument = f' --postgres-archive "$bundle_dir/{pg_path}"' if flavor == "offline" else ""
        wrapper = '#!/bin/sh\nset -eu\nbundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "$bundle_dir/bin/ygg" init' + argument + ' "$@"\n'
        files["initialize"] = wrapper.encode(), 0o755
        readme = f'''Yggdrasil {version}: {target}, {flavor} bundle

Extract into a new directory and run ./initialize. It preserves configured
external databases; the offline PostgreSQL argument requires managed mode.
The online bundle downloads the exact PostgreSQL archive in postgres-packages.json
only during explicit managed initialization. The offline bundle includes it.
For unattended database-only setup: ./initialize --yes --skip tmux,jq,rtk,hooks,project
The skip list avoids optional dependency installers and hook/project changes.
Other Yggdrasil commands remain available through ./bin/ygg; keep this directory
at a stable path while its managed supervisor is running.

GNU Linux needs libossp-uuid, libxml2, liblz4, libzstd and distribution runtime
libraries. macOS binaries retain their supplied signatures. This package does not
claim Developer ID signing, notarization or quarantine qualification. Never clear
quarantine automatically. Existing release gates must pass before distribution.

manifest.json binds file digests and records source provenance. It is not a
signature: verify the downloaded bundle against a trusted release checksum.
PostgreSQL/theseus-rs notices are in licenses; macOS PG bundles OpenSSL under the
included Apache 2.0 license. GNU Linux links system libraries.
'''
        files["README.txt"] = readme.encode(), 0o644
        manifest = {"schema": 1, "version": version, "target": target, "flavor": flavor,
                    "build_source_commit": commit, "build_source_dirty": dirty,
                    "postgres_release": release, "files": {
                        name: {"sha256": sha(data), "bytes": len(data), "mode": mode}
                        for name, (data, mode) in sorted(files.items())}}
        files["manifest.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode(), 0o644
        name = f"ygg-{version}-{target}-{flavor}.tar.gz"
        data = tar_bytes(files, epoch)
        publish(output / name, data)
        checksums[name] = sha(data)
    publish(output / "SHA256SUMS", "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items())).encode())
    print(json.dumps({"directory": str(output), "files": checksums, "source_dirty": dirty}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--postgres-archive", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    assemble(args.binary, args.postgres_archive, args.target, args.output)
