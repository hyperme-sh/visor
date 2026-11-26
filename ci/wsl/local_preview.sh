#!/usr/bin/env bash
set -euo pipefail
base=/home/visorci/.visor-wsl
install=/opt/visor-wsl
owner=
editor=
cleanup() {
    result=$?
    if [ "$result" -ne 0 ]; then
        tail -35 "$base/owner.log" "$base/editor.log" 2>/dev/null || true
    fi
    if [ -n "$editor" ]; then kill "$editor" 2>/dev/null || true; fi
    if [ -n "$owner" ]; then kill "$owner" 2>/dev/null || true; fi
}
trap cleanup EXIT

VISOR_FIXTURE_PREVIEW=local python3 "$install/task.py" >"$base/owner.log" 2>&1 &
owner=$!
for _ in {1..60}; do
    test ! -s "$base/case.json" || break
    kill -0 "$owner"
    sleep .5
done
readarray -t details < <(python3 - "$base/case.json" <<'PY'
from pathlib import Path
import json
import sys

case = json.loads(Path(sys.argv[1]).read_text())
for name in ("workspace", "cliTask", "marker"):
    print(case[name])
PY
)
profile="$base/editor-profile"
extensions="$base/editor-extensions"
install -d -o visorci -g visorci "$profile/User" "$extensions"
cat >"$profile/User/settings.json" <<'JSON'
{
  "hypermesh.companionExecutable": "/opt/visor-wsl/hypermesh-companion",
  "workbench.startupEditor": "none",
  "editor.accessibilitySupport": "on",
  "security.workspace.trust.startupPrompt": "always",
  "extensions.autoUpdate": false,
  "update.mode": "none",
  "telemetry.telemetryLevel": "off",
  "window.dialogStyle": "custom"
}
JSON
code_args=(--no-sandbox --user-data-dir="$profile" --extensions-dir="$extensions")
runuser -u visorci -- /opt/visor-code/bin/code "${code_args[@]}" \
    --install-extension "$install/hypermesh-vscode.vsix" --force
runuser -u visorci -- env HYPERMESH_RUNTIME_DIR="$base/runtime" xvfb-run -a \
    /opt/visor-code/code "${code_args[@]}" --disable-gpu --disable-dev-shm-usage \
    --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 \
    --skip-welcome --skip-release-notes --locale=en-US "${details[0]}" >"$base/editor.log" 2>&1 &
editor=$!
result=$("$install/node" "$install/editor.mjs" 9222 local "${details[@]}" '' preview)
printf '%s\n' "$result"
preview=$(sed -n 's/^VISOR_EDITOR_PREVIEW=//p' <<< "$result")
[[ "$preview" =~ ^[a-f0-9-]{36}$ ]]
runuser -u visorci -- python3 "$install/task.py" preview-open "$preview"
runuser -u visorci -- python3 "$install/task.py" preview-closed "$preview"
