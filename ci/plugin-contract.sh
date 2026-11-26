#!/usr/bin/env bash
# Build outside the checkout and install packages using the documented layout.
set -euo pipefail
umask 077
repo="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/packages" "$scratch/staging" "$scratch/author"
cp -R "$repo/plugins/sdk" "$repo/plugins/reference" "$repo/plugins/github-issue" "$scratch/author/"
target="$(rustc -vV | sed -n 's/^host: //p')"
(
    cd "$scratch/author/reference"
    CARGO_TARGET_DIR="$scratch/build" RUSTFLAGS='-C target-feature=+crt-static' \
        cargo build --release --locked --offline --target "$target"
)
install -D -m 600 "$scratch/build/$target/release/reference-byte-count" \
    "$scratch/packages/current/plugin"

for mode in v0 mcp invalid-output isolation slow; do
    mkdir "$scratch/packages/$mode"
    VISOR_PLUGIN_FIXTURE="$mode" rustc --edition 2021 -C target-feature=+crt-static -C strip=symbols \
        "$repo/plugins/fixtures/stdio.rs" -o "$scratch/packages/$mode/plugin"
done
install -D -m 600 "$scratch/packages/current/plugin" "$scratch/packages/incompatible/plugin"

python3 - "$repo/plugins/reference/manifest.template.json" "$scratch/packages" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

template, packages = map(Path, sys.argv[1:])
for package in packages.iterdir():
    package.chmod(0o700)
    executable = package / "plugin"
    executable.chmod(0o600)
    manifest = json.loads(template.read_text())
    if package.name == "v0":
        manifest["api_versions"] = [0]
    if package.name == "incompatible":
        manifest["api_versions"] = [999]
    if package.name == "mcp":
        manifest["profile"] = "local-mcp-stdio"
    manifest["executable_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
    (package / "manifest.json").write_text(json.dumps(manifest) + "\n")
PY

# Fail with the runner's namespace error before invoking the production guard,
# which deliberately discards subprocess stderr.
install -m 500 "$scratch/packages/current/plugin" "$scratch/probe"
env -i /usr/bin/bwrap --unshare-all --unshare-user --disable-userns --die-with-parent --new-session \
    --clearenv --cap-drop ALL --ro-bind "$scratch/probe" /plugin --remount-ro / \
    --chdir / -- /plugin <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"plugin_id":"org.example.byte-count","api_versions":[1,0]}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"count-bytes","input":{"content_base64":"YWJj"},"timeout_ms":4000}}
JSON

cd "$repo"
cargo build --locked --no-default-features --features headless --bin hypermesh-plugin-conformance --bin hypermesh-plugin-publish
python3 - "$scratch" "$repo/target/debug/hypermesh-plugin-conformance" <<'PY'
import hashlib
import json
from pathlib import Path
import subprocess
import sys

scratch, runner = map(Path, sys.argv[1:])
for mode, expected in {
    "current": 1, "v0": 0, "mcp": 1, "isolation": 1,
    "invalid-output": "invalid_output", "incompatible": "incompatible_api",
    "slow": "plugin_timeout",
}.items():
    manifest = scratch / "packages" / mode / "manifest.json"
    digest = hashlib.sha256(manifest.read_bytes()).hexdigest()
    result = subprocess.run(
        [runner, manifest, digest, "count-bytes", scratch / "staging"],
        input='{"content_base64":"YWJj"}', text=True,
        capture_output=True, timeout=15, check=False,
    )
    if isinstance(expected, int):
        assert result.returncode == 0, (mode, result.stderr)
        assert json.loads(result.stdout) == {"api_version": expected, "result": {"byte_count": 3}}, mode
    else:
        assert result.returncode != 0 and result.stderr.startswith(expected + ":"), (mode, result.stderr)
    assert not list((scratch / "staging").iterdir()), mode
    print(f"{mode}: passed")
PY
openssl genpkey -algorithm Ed25519 -outform DER -out "$scratch/author-key.der"
"$repo/target/debug/hypermesh-plugin-publish" "$scratch/packages/current" \
    https://packages.example.org/byte-count.visor-plugin < "$scratch/author-key.der" > "$scratch/author.visor-plugin"
VISOR_PLUGIN_PUBLICATION="$scratch/author.visor-plugin" VISOR_PLUGIN_CONTRACT_DIR="$scratch/packages" cargo test --locked --no-default-features --features headless --lib \
    api::tests::plugin_contract::independent_package_uses_owner_admission -- --ignored --exact

(
    cd "$scratch/author/github-issue"
    CARGO_TARGET_DIR="$scratch/build" RUSTFLAGS='-C target-feature=+crt-static' \
        cargo build --release --locked --offline --target "$target"
)
install -D -m 600 "$scratch/build/$target/release/visor-github-issue" "$scratch/github-issue/plugin"
python3 - "$repo/plugins/github-issue/manifest.template.json" "$scratch/github-issue" <<'PYTHON'
import hashlib
import json
from pathlib import Path
import sys

template, package = map(Path, sys.argv[1:])
manifest = json.loads(template.read_text())
manifest["executable_sha256"] = hashlib.sha256((package / "plugin").read_bytes()).hexdigest()
(package / "manifest.json").write_text(json.dumps(manifest) + "\n")
PYTHON
"$repo/target/debug/hypermesh-plugin-publish" "$scratch/github-issue" \
    https://packages.example.org/github-issue.visor-plugin < "$scratch/author-key.der" > "$scratch/github-issue.visor-plugin"
VISOR_GITHUB_ISSUE_PUBLICATION="$scratch/github-issue.visor-plugin" cargo test --locked --no-default-features --features headless --lib \
    api::tests::plugin_contract::github_issue_plugin_uses_the_selected_account_and_public_repository -- --ignored --exact
