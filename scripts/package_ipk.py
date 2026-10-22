#!/usr/bin/env python3
"""Package the launcher with Rust Ares and retain its Homebrew manifest."""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parent.parent
APP_DIR = ROOT_DIR / "webos-app"
OUTPUT_DIR = ROOT_DIR / "target"


def build_ipk(app_dir=APP_DIR, output_dir=OUTPUT_DIR):
    app_dir, output_dir = Path(app_dir).resolve(), Path(output_dir).resolve()
    info = json.loads((app_dir / "appinfo.json").read_text(encoding="utf-8"))
    if not isinstance(info, dict):
        raise ValueError("appinfo.json must contain a JSON object")
    for key in ("id", "version", "type", "title", "appDescription"):
        if not isinstance(info.get(key), str) or not info[key]:
            raise ValueError(f"appinfo.json requires {key}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", info["id"]):
        raise ValueError("invalid application id")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", info["version"]):
        raise ValueError("application version must contain three numeric components")
    packager = shutil.which("ares-rs-package")
    if not packager:
        raise RuntimeError("ares-rs-package is required; install the Rust ares-cli-rs tools")
    output_dir.mkdir(parents=True, exist_ok=True)
    output_ipk = output_dir / f"{info['id']}_{info['version']}_all.ipk"
    output_manifest = output_dir / f"{info['id']}.manifest.json"
    with tempfile.TemporaryDirectory(prefix=".ares-package-", dir=output_dir) as temporary:
        staging = Path(temporary)
        subprocess.run(
            [packager, str(app_dir), "--outdir", str(staging), "--force-arch", "all"],
            check=True, timeout=120, stdout=sys.stderr, stderr=sys.stderr,
        )
        package = staging / output_ipk.name
        if package.is_symlink() or not package.is_file():
            raise RuntimeError(f"Ares did not produce the expected package: {output_ipk.name}")
        manifest = {
            "id": info["id"], "version": info["version"], "type": info["type"],
            "title": info["title"], "appDescription": info["appDescription"],
            "iconUri": "https://raw.githubusercontent.com/adeze/lg-hue-sync/main/webos-app/icon130.png",
            "sourceUrl": "https://github.com/adeze/lg-hue-sync",
            "rootRequired": True, "ipkUrl": output_ipk.name,
            "ipkHash": {"sha256": hashlib.sha256(package.read_bytes()).hexdigest()},
            "ipkSize": package.stat().st_size,
        }
        staged_manifest = staging / output_manifest.name
        staged_manifest.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        os.replace(package, output_ipk)
        os.replace(staged_manifest, output_manifest)
    return output_ipk, output_manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--print-path", action="store_true", help="package and print only the exact IPK path to stdout")
    args = parser.parse_args()
    try:
        package, manifest = build_ipk()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Packaging failed: {error}\n")
    if args.print_path:
        print(package)
    else:
        print(f"Successfully created {package} ({package.stat().st_size} bytes)")
        print(f"Successfully created {manifest}")


if __name__ == "__main__":
    main()
