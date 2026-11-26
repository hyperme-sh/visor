#!/usr/bin/env python3
"""Deliver a credential encrypted to the two native CI jobs in one run."""
import argparse
import base64
import io
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time
import zipfile


REPO = "hyperme-sh/visor"
WORKFLOW = "live-github-issue.yml"
ARCHES = ("amd64", "arm64")


def api(path):
    return subprocess.check_output(["gh", "api", f"repos/{REPO}/actions/{path}"])


def recipient_keys(run_id, attempt):
    names = {f"github-issue-recipient-{attempt}-{arch}": arch for arch in ARCHES}
    deadline = time.monotonic() + 240
    while time.monotonic() < deadline:
        artifacts = json.loads(api(f"runs/{run_id}/artifacts"))["artifacts"]
        keys = {}
        for artifact in artifacts:
            name = artifact["name"]
            if name in names and not artifact["expired"]:
                with zipfile.ZipFile(io.BytesIO(api(f"artifacts/{artifact['id']}/zip"))) as archive:
                    keys[names[name]] = archive.read("github-issue-recipient.pem")
        if len(keys) == len(ARCHES):
            return keys
        time.sleep(5)
    raise SystemExit("Both native runner keys must be available before delivery.")


def seal(key, token):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "recipient.pem"
        path.write_bytes(key)
        encrypted = subprocess.run([
            "openssl", "pkeyutl", "-encrypt", "-pubin", "-inkey", str(path),
            "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha256",
            "-pkeyopt", "rsa_mgf1_md:sha256",
        ], input=token, capture_output=True, check=True).stdout
    if len(encrypted) != 384:
        raise SystemExit("Expected a 3072-bit CI runner key.")
    return base64.b64encode(encrypted).decode("ascii")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_id", type=int)
    parser.add_argument("credential_file", type=Path)
    args = parser.parse_args()
    run = json.loads(api(f"runs/{args.run_id}"))
    if (run["head_branch"] != "main" or run["head_repository"]["full_name"] != REPO
            or run["path"].split("@", 1)[0] != f".github/workflows/{WORKFLOW}"
            or run["event"] != "workflow_dispatch" or run["status"] not in {"queued", "in_progress"}):
        raise SystemExit("Select an active Visor live contract run on main.")

    attempt = run["run_attempt"]
    keys = recipient_keys(args.run_id, attempt)
    with args.credential_file.open("rb") as file:
        token = file.read(512).strip()
    if not re.fullmatch(rb"github_pat_[A-Za-z0-9_]{20,255}", token):
        raise SystemExit("The credential file must contain a fine-grained GitHub Issues token.")
    inputs = {"receiver": f"{args.run_id}-{attempt}"}
    inputs.update({f"{arch}_envelope": seal(key, token) for arch, key in keys.items()})
    subprocess.run(["gh", "workflow", "run", WORKFLOW, "--repo", REPO, "--ref", "main", "--json"],
                   input=json.dumps(inputs), text=True, check=True)
    print(f"Delivered the encrypted envelopes for run {args.run_id}, attempt {attempt}.")


if __name__ == "__main__":
    main()
