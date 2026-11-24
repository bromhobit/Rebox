#!/usr/bin/env python3
"""Rebox release: package prebuilt ARM64 iOS binaries, never user sessions.

The iOS cross-compiled Mach-O inputs must be provided explicitly by the user or
an external reproducible build workflow. This packager does not read or reuse
anything from iPhone data containers or Apple sessions.
"""
from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[1]


def main() -> None:
    parser = argparse.ArgumentParser(description="Create Rebox IPA + rootless .deb for iOS arm64")
    parser.add_argument("--binary-dir", type=Path, required=True, help="Dir of cross-built ARM64 Mach-O executables")
    parser.add_argument("--output", type=Path, default=PROJECT / "dist", help="Release output directory")
    parser.add_argument("--version", default="1.0.1")
    parser.add_argument("--force", action="store_true", help="Overwrite existing output .deb/.ipa files")
    args = parser.parse_args()

    specs = {
        "app": "rebox-ui",
        "controller": "rebox-controller",
        "profilectl": "profilectl",
        "apple": "rebox-apple",
    }
    bin_dir = args.binary_dir.resolve(strict=True)
    for label, name in specs.items():
        target = bin_dir / name
        if target.is_symlink() or not target.is_file() or not target.stat().st_size:
            parser.error(f"Missing or symlinked {label} ARM64 Mach-O: {target}")
        print("INPUT",label,name,"size",target.stat().st_size)
    args.output.mkdir(parents=True, exist_ok=True)
    command = [sys.executable,str(PROJECT / "package" / "build.py"),
               "--app-binary",str(bin_dir / specs["app"]),
               "--controller-binary",str(bin_dir / specs["controller"]),
               "--profilectl-binary",str(bin_dir / specs["profilectl"]),
               "--apple-binary",str(bin_dir / specs["apple"]),
               "--signer","builtin", "--output", str(args.output), "--version", args.version]
    if args.force:
        command.append("--force")
    subprocess.run(command,check=True)
    artifacts = [args.output/f"Rebox-{args.version}-rootless.deb",args.output/f"Rebox-{args.version}.ipa"]
    lines=[]
    for path in artifacts:
        digest=hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{digest}  {path.name}")
        print("BUILT",path.name,path.stat().st_size,"bytes")
    checksums=args.output/"SHA256SUMS"
    checksums.write_text("\n".join(lines)+"\n",encoding="utf-8")
    print("SHA256SUMS",checksums)

if __name__ == "__main__":
    main()
