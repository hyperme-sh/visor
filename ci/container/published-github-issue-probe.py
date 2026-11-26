"""Verify the real daemon, session grant, and bundled sandbox inside TEST."""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

sys.path.insert(0, "/usr/local/bin")
from visor_unattended import Connection

PLUGIN = "dev.visor.github-issue"
BUNDLE = Path("/usr/local/share/hypermesh-visor/plugins/github-issue.visor-plugin")
USER_PACKAGE = Path(os.environ["HYPERMESH_CONFIG_DIR"]) / "plugins-installed" / f"{PLUGIN}.package"


def request(method, path, value=None, companion=True):
    conn = Connection(companion)
    try:
        conn.request(method, path, None if value is None else json.dumps(value),
                     {"Content-Type": "application/json"})
        response = conn.getresponse()
        data = response.read(65537)
        assert len(data) <= 65536
        return response.status, json.loads(data) if data else None
    finally:
        conn.close()


def expect(status, method, path, value=None, companion=True):
    actual, body = request(method, path, value, companion)
    assert actual == status, (method, path, status, actual, body)
    return body


def sandbox_diagnostic():
    # Bubblewrap's stderr is deliberately private in the daemon. Reapply the
    # same sandbox launch to the public bundled binary only, in this container.
    with tempfile.TemporaryDirectory(prefix="visor-plugin-probe-") as folder:
        executable = Path(folder) / "plugin"
        executable.write_bytes(base64.b64decode(json.loads(BUNDLE.read_bytes())["executable"], validate=True))
        executable.chmod(0o500)
        result = subprocess.run([
            "/usr/bin/bwrap", "--unshare-all", "--unshare-user", "--disable-userns",
            "--die-with-parent", "--new-session", "--clearenv", "--cap-drop", "ALL",
            "--ro-bind", str(executable), "/plugin", "--remount-ro", "/", "--chdir", "/",
            "--", "/plugin", "--third-party-notices",
        ], text=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10)
        print(f"Bubblewrap exit {result.returncode}    {result.stderr.strip()}", flush=True)
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith(("CapEff:", "NoNewPrivs:", "Seccomp:")):
            print(line, flush=True)


assert not USER_PACKAGE.exists()
catalog = expect(200, "GET", "/plugins")["plugins"]
entry, = [row for row in catalog if row["metadata"]["id"] == PLUGIN]
assert entry["bundled"] is True and entry["enabled"] is True
print("Bundled plugin discovered through the owner socket with an empty user plugin store.", flush=True)

session = expect(201, "POST", "/session", {
    "purpose": "Published TEST plugin contract",
    "recipes": [],
    "agent": {"name": "Container contract", "instructions": ""},
    "skills": [],
    "secrets": [{"name": "github-issues-token", "source": "local",
                 "value": "github_pat_visor_container_fixture_123456789"}],
}, companion=False)["id"]
grants = f"/session/{session}/plugins"
grant_path = f"{grants}/{PLUGIN}"
code_path = f"{grant_path}/github-issue/code"
script = {"code": "return 6 * 7;"}
grant = {"plugin_id": PLUGIN, "revision": entry["revision"], "tool": "read-issue",
         "target": {"account": 21, "repository": "Example/Project"}}
expect(200, "GET", "/companion")
expect(204, "POST", "/companion/automation", {"action": "start", "expected_session": session})
try:
    expect(404, "POST", code_path, script)
    expect(201, "POST", grants, grant)
    denied = expect(403, "POST", code_path, script)
    assert "opted-in GitHub issue grant" in json.dumps(denied)
    expect(204, "DELETE", grant_path)
    print("Code Mode refused both a missing grant and a grant that omitted opt-in.", flush=True)
    expect(201, "POST", grants, {**grant, "code_mode": True})
    status, result = request("POST", code_path, script)
    print(f"Opted-in Code Mode returned HTTP {status}    {json.dumps(result)}", flush=True)
    if status != 200:
        sandbox_diagnostic()
        raise SystemExit(1)
    assert result["result"] == 42 and result["calls"] == 0
    assert not USER_PACKAGE.exists()
finally:
    expect(204, "DELETE", f"/session/{session}", companion=False)
