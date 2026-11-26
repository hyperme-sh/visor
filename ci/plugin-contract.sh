#!/usr/bin/env bash
# Build outside the checkout and install packages using the documented layout.
set -euo pipefail
umask 077
repo="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/packages" "$scratch/staging" "$scratch/author"
cp -R "$repo/plugins/sdk" "$repo/plugins/reference" "$scratch/author/"
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

cd "$repo"
cargo build --locked --bin hypermesh-plugin-conformance
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
VISOR_PLUGIN_CONTRACT_DIR="$scratch/packages" cargo test --locked --lib \
    api::tests::plugin_contract::independent_package_uses_owner_admission -- --ignored --exact
