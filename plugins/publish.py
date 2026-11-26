#!/usr/bin/env python3
"""Build a signed Visor publication from an existing plugin package."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


DOMAIN = b"visor-plugin-publication-v1\0"
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")


def encode(data):
    return base64.b64encode(data).decode("ascii")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read(path, limit):
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    if not 0 < len(data) <= limit:
        raise ValueError(f"{path.name} is empty or too large")
    return data


def publish(package, signing_key, source, publisher_domain):
    manifest = read(package / "manifest.json", 64 * 1024)
    executable = read(package / "plugin", 8 * 1024 * 1024)
    definition = json.loads(manifest)
    if definition["executable_sha256"] != digest(executable):
        raise ValueError("manifest executable digest does not match plugin")
    public = subprocess.check_output(
        ["openssl", "pkey", "-in", signing_key, "-pubout", "-outform", "DER"]
    )
    if not public.startswith(ED25519_SPKI) or len(public) != len(ED25519_SPKI) + 32:
        raise ValueError("publisher key must be Ed25519")
    publisher_key = encode(public[len(ED25519_SPKI):])
    statement = json.dumps({
        "version": 1,
        "plugin_id": definition["id"],
        "package_version": definition["package_version"],
        "source": source,
        "manifest_sha256": digest(manifest),
        "publisher_key": publisher_key,
        "publisher_domain": publisher_domain,
    }, separators=(",", ":")).encode()
    with tempfile.TemporaryDirectory(prefix="visor-plugin-sign-") as directory:
        unsigned = Path(directory) / "message"
        unsigned.write_bytes(DOMAIN + statement)
        signature = subprocess.check_output(
            ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", signing_key, "-in", unsigned]
        )
    envelope = {
        "statement": encode(statement),
        "signature": encode(signature),
        "manifest": encode(manifest),
        "executable": encode(executable),
    }
    artifact = json.dumps(envelope, separators=(",", ":")).encode()
    return artifact, publisher_key


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", required=True, type=Path)
    parser.add_argument("--key", required=True, type=Path)
    parser.add_argument("--source", required=True)
    parser.add_argument("--publisher-domain")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        artifact, public_key = publish(args.package, args.key, args.source, args.publisher_domain)
        fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(artifact)
        print(json.dumps({"publication_sha256": digest(artifact), "publisher_key": public_key}))
    except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"Could not build Visor publication: {exc}\n")


if __name__ == "__main__":
    main()
