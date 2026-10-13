#!/usr/bin/env python3
"""Read-only macOS signature/Gatekeeper audit of an offline candidate bundle.

Does not sign, notarize, execute payloads, change quarantine or alter system policy.
A pass is static distribution evidence, not a clean-machine first-launch test.
"""
import argparse
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import platform
import struct
import subprocess
import tarfile
import tempfile
import sys

sys.dont_write_bytecode = True


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def inspect(name, data, directory):
    # Supported native bundles contain thin 64-bit little-endian Mach-O images.
    if data[:4] in (b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
                     b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
                     b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        raise ValueError("unsupported Mach-O representation in distribution")
    if len(data) < 16 or data[:4] != b"\xcf\xfa\xed\xfe":
        return None
    executable = struct.unpack_from("<I", data, 12)[0] == 2
    # Never replace an assessed inode; avoid signature/Gatekeeper cache aliasing.
    path = directory / hashlib.sha256(data).hexdigest()
    if not path.exists():
        path.write_bytes(data)
        path.chmod(0o700)
    checks = []
    for args in (["codesign", "--verify", "--strict"],
                 ["codesign", "--display", "--verbose=4"]):
        result = subprocess.run(args + [str(path)], capture_output=True, text=True, timeout=60)
        checks.append({"command": args, "returncode": result.returncode,
                       "output": (result.stdout + result.stderr).replace(str(path), name)})
    metadata = checks[1]["output"]
    developer_id = any(line.startswith("Authority=Developer ID Application:")
                       for line in metadata.splitlines())
    team = any(line.startswith("TeamIdentifier=") and line != "TeamIdentifier=not set"
               for line in metadata.splitlines())
    hardened = any(line.startswith("CodeDirectory ") and "runtime" in line
                   for line in metadata.splitlines())
    if executable:
        args = ["spctl", "--assess", "--type", "execute", "--verbose=4"]
        result = subprocess.run(args + [str(path)], capture_output=True, text=True, timeout=60)
        checks.append({"command": args, "returncode": result.returncode,
                       "output": (result.stdout + result.stderr).replace(str(path), name)})
    return {"path": name, "sha256": hashlib.sha256(data).hexdigest(),
            "executable": executable, "developer_id": developer_id, "team_identifier": team,
            "hardened_runtime": hardened, "checks": checks,
            "passed": developer_id and team and (hardened or not executable)
                      and all(c["returncode"] == 0 for c in checks)}


def audit(bundle):
    if platform.system() != "Darwin":
        raise ValueError("run the distribution audit on macOS")
    smoke = load("smoke-release-bundle")
    manifest, files = smoke.verified_files(bundle)
    target = manifest.get("target", "")
    if manifest["flavor"] != "offline" or target != load("build-release-bundles").native_target():
        raise ValueError("audit an offline bundle on its native macOS architecture")
    package = next(p for p in manifest["postgres_release"]["packages"] if p["target"] == target)
    archive = files[f'postgres/{package["root"]}.tar.gz'][0]
    if len(archive) != package["bytes"] or hashlib.sha256(archive).hexdigest() != package["sha256"]:
        raise ValueError("nested PostgreSQL archive differs from source pin")
    rows = []
    with tempfile.TemporaryDirectory(prefix="ygg-signature-audit-") as tmp:
        directory = Path(tmp)
        cli = inspect("bin/ygg", files["bin/ygg"][0], directory)
        if cli is None or not cli["executable"]:
            raise ValueError("bundle CLI is not a supported Mach-O executable")
        rows.append(cli)
        total = 0
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            for index, member in enumerate(source):
                if index >= 20000:
                    raise ValueError("nested archive exceeds member limit")
                if not member.isfile():
                    continue
                total += member.size
                if not 0 <= member.size <= 128 * 1024 * 1024 or total > 256 * 1024 * 1024:
                    raise ValueError("nested archive exceeds audit byte limit")
                data = source.extractfile(member).read()
                row = inspect(member.name, data, directory)
                if row:
                    rows.append(row)
    if not any(row["path"] == f'{package["root"]}/bin/postgres' and row["executable"]
               for row in rows):
        raise ValueError("PostgreSQL server is not a supported Mach-O executable")
    return {"schema": 1, "target": target, "macos": platform.mac_ver()[0],
            "bundle_sha256": hashlib.sha256(bundle.read_bytes()).hexdigest(),
            "source_commit": manifest["build_source_commit"], "images": rows,
            "passed": all(row["passed"] for row in rows),
            "scope": "Static signature and executable Gatekeeper assessments only; quarantine first-launch and offline ticket availability still require separate rehearsal."}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--report", required=True, type=Path, help="new JSON report path; never overwrites")
    args = parser.parse_args()
    report = audit(args.bundle)
    with args.report.open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps({"passed": report["passed"], "images": len(report["images"]),
                      "report": str(args.report)}))
    raise SystemExit(0 if report["passed"] else 1)
