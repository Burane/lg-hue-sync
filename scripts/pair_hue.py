#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Deprecated compatibility wrapper for the Rust HTTPS pairing command.

Usage: uv run scripts/pair_hue.py [--bridge-ip <bridge-ip>] [--output-config config.json]
Use `cargo run --locked -- pair` directly for new workflows.
"""

import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bridge-ip", help="Manual Hue Bridge IP address")
    parser.add_argument("--output-config", default="config.json", help="Credential configuration path")
    parser.add_argument("--tv-ip", help="Removed: direct Hyperion configuration is unsupported")
    args = parser.parse_args()
    if args.tv_ip:
        parser.error("--tv-ip is no longer supported; this wrapper pairs LG Hue Sync only")

    repo = Path(__file__).resolve().parent.parent
    output = Path(args.output_config).resolve()
    version = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True)
    host = next((line.removeprefix("host: ") for line in version.stdout.splitlines() if line.startswith("host: ")), None)
    if not host:
        parser.error("rustc did not report its host target")
    # An explicit host target overrides a shell's cross-compilation target.
    command = ["cargo", "run", "--locked", "--target", host, "--manifest-path", str(repo / "Cargo.toml"), "--", "pair", "--output", str(output)]
    if args.bridge_ip:
        command.extend(["--bridge", args.bridge_ip])
    subprocess.run(command, cwd=repo, check=True)


if __name__ == "__main__":
    main()
