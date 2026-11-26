#!/usr/bin/env python3
"""Collect the tested Windows and macOS assets for the current Visor revision."""

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import tomllib
import zipfile


def verify(directory, manifest, names):
    entries = {}
    for line in (directory / manifest).read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64}) [ *]([A-Za-z0-9_.-]+)", line)
        if not match or match[2] in entries:
            raise ValueError(f"Invalid checksum entry in {directory.name}")
        entries[match[2]] = match[1]
    if set(entries) != names:
        raise ValueError(f"Unexpected files in {directory.name} manifest")
    for name, checksum in entries.items():
        with (directory / name).open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != checksum:
                raise ValueError(f"Checksum mismatch for {name}")
    return entries


def assemble(output, candidates, revision, version):
    assets = {}
    for platform, architectures in (("windows", ("x64", "arm64")), ("macos", ("arm64", "x86_64"))):
        for arch in architectures:
            stem = f"hypermesh-visor-{platform}-{arch}"
            directory = candidates / f"visor-desktop-release-{platform}-{arch}"
            if platform == "windows":
                archive = f"{stem}-unsigned.zip"
                installer = f"hypermesh-visor-{version}-windows-{arch}-unsigned-setup.exe"
                checksums = verify(directory, "SHA256SUMS", {archive, installer})
                source_file = "visor-source-revision.txt"
            else:
                archive = f"{stem}.zip"
                checksums = verify(directory, f"{archive}.sha256", {archive})
                source_file = f"{stem}/source-revision"
            with zipfile.ZipFile(directory / archive) as bundle:
                if bundle.read(source_file).decode("ascii").strip() != revision:
                    raise ValueError(f"Source revision mismatch for {archive}")
            assets.update({name: (directory / name, checksum) for name, checksum in checksums.items()})

    output.mkdir()
    for name, (source, _) in assets.items():
        shutil.copyfile(source, output / name)
    (output / "checksums.txt").write_text(
        "".join(f"{assets[name][1]}  {name}\n" for name in sorted(assets))
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("candidates", type=Path)
    parser.add_argument("revision")
    args = parser.parse_args()
    with (Path(__file__).resolve().parents[1] / "Cargo.toml").open("rb") as stream:
        version = tomllib.load(stream)["package"]["version"]
    assemble(args.output, args.candidates, args.revision, version)
