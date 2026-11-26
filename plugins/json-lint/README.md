# Visor JSON lint

Run strict JSON lint on one saved file in an assigned local Linux workspace.
The [public plugin API](../README.md) gives the linter the admitted file
contents through the `workspace.read` scope. The `json-strict-v1`
configuration checks JSON syntax, UTF-8 encoding, and duplicate object keys.
It handles files up to 32 KiB and reports up to 32 findings per invocation.

## Prepare the package

From the Visor checkout, build the package for the machine running Visor.
The local development package uses the same manifest and runner as signed
installations.

```sh
target="$(rustc -vV | sed -n 's/^host: //p')"
package="$(mktemp -d)"
CARGO_TARGET_DIR="$PWD/target/json-lint" RUSTFLAGS='-C target-feature=+crt-static' \
  cargo build --manifest-path plugins/json-lint/Cargo.toml --release --locked --target "$target"
install -m 600 "target/json-lint/$target/release/visor-json-lint" "$package/plugin"
(
  umask 077
  python3 - "$package" <<'PYTHON'
import hashlib
import json
from pathlib import Path
import sys

package = Path(sys.argv[1])
manifest = json.loads(Path("plugins/json-lint/manifest.template.json").read_text())
manifest["executable_sha256"] = hashlib.sha256((package / "plugin").read_bytes()).hexdigest()
(package / "manifest.json").write_text(json.dumps(manifest) + "\n")
PYTHON
)
sha256sum "$package/manifest.json"
```

Select and approve the local Visor session for the workspace. Use **Workspace
permissions** to save a read rule on the exact absolute file path. Review
the manifest and run the following with the selected session ID, the printed
manifest digest, and that same absolute file path.

```sh
hypermesh-companion --plugins grant-local "$session" "$package/manifest.json" "$manifest_sha256" lint-json "$workspace/$file"
```

If using an installed signed package, use `--plugins grant` with the installed
plugin ID and revision instead. The host must admit the same scoped read on
each run.

## Run the linter

```sh
hypermesh-companion --workspace-lint "$workspace" "$file"
```

Pass a workspace-relative `.json` path as `file`. The command writes a JSON
report with the file, runtime, plugin version, configuration, exact input
digest, and diagnostics. Lines and UTF-16 columns start at 1. Exit status
`0` means clean, `1` means diagnostics were found, and `2` means the requested
lint could not run. Status `2` includes an error message in the JSON output.

In VS Code, open the same saved file in a trusted local workspace and run
**Hypermesh: Lint current JSON file with Visor**. Select a finding in
**Problems** to open its location.
