"""Disposable WSL fixture for the real daemon and companion binaries."""
import http.client
import http.server
import json
import os
from pathlib import Path
import pwd
import re
import socket
import socketserver
import subprocess
import sys
import time
import uuid

USER = pwd.getpwnam("visorci")
BASE = Path(USER.pw_dir) / ".visor-wsl"
WORKSPACE = BASE / "project"
INSTALL = Path("/opt/visor-wsl")
OWNER = BASE / "owner.sock"
COMPANION = BASE / "runtime/companion.sock"
MARKER = "visor-wsl-scoped-task"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=3)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.path))


def request(path, expected, value=None):
    conn = Connection(COMPANION)
    try:
        conn.request("GET" if value is None else "POST", path,
                     None if value is None else json.dumps(value),
                     {"Content-Type": "application/json", "Connection": "close"})
        response = conn.getresponse()
        data = response.read(65537)
        assert response.status == expected, (path, response.status, data)
        return json.loads(data) if data else None
    finally:
        conn.close()


def dump(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value), encoding="utf-8")
    temporary.replace(path)


def companion(*args, check=True, device=None):
    host = ["--host", device] if device else []
    return subprocess.run(
        [str(INSTALL / "hypermesh-companion"), "--terminal-task", str(WORKSPACE), *host, *args],
        capture_output=True, text=True, check=check, timeout=40,
    )


def verify_task(task_id):
    case = json.loads((BASE / "case.json").read_text())
    device = case.get("device")
    result = companion("watch", str(uuid.UUID(task_id)), device=device)
    assert result.stdout.strip() == MARKER, result.stdout
    assert "completed" in result.stderr and "exit 0" in result.stderr, result.stderr
    assert (WORKSPACE / "runs").read_text() == "xx", "Expected one task dispatch from each client"
    print(f"CLI watched VS Code task {task_id} in {os.environ['WSL_DISTRO_NAME']} at {WORKSPACE}", flush=True)


def client():
    assert os.getuid() == USER.pw_uid
    device = os.environ.get("VISOR_SSH_FIXTURE_DEVICE")
    tenant = os.environ.get("VISOR_SSH_FIXTURE_TENANT")
    assert os.environ["WSL_DISTRO_NAME"].startswith("VisorCI-")
    assert "microsoft" in os.uname().release.lower(), os.uname().release
    with open(BASE / "config/terminal-task.json", "x",
              opener=lambda path, flags: os.open(path, flags, 0o600)) as config:
        json.dump({
            "workspace": str(WORKSPACE), "cwd": str(WORKSPACE), "command": "/bin/sh",
            "args": ["-c", f"printf x >> runs; printf '%s\\n' {MARKER}"],
            "timeout_seconds": 10,
        }, config)
    with subprocess.Popen([
        str(INSTALL / "hypermesh-visor"), "--fixture",
        "--fixture-log", str(BASE / "desktop.log"),
    ]) as daemon:
        try:
            for _ in range(100):
                if COMPANION.is_socket():
                    break
                if daemon.poll() is not None:
                    raise RuntimeError(f"Visor exited at startup ({daemon.returncode})")
                time.sleep(0.1)
            else:
                raise RuntimeError("Visor companion socket was unavailable")
            session = request("/session", 201, {
                "purpose": "WSL terminal fixture", "recipes": [], "skills": [],
                "agent": {"name": "WSL fixture", "instructions": ""},
            })["id"]
            dump(BASE / "assignment.json", {
                "workspace": str(WORKSPACE), "identity": {
                    **{key: str(uuid.uuid4()) for key in
                       ("bot_id", "assignment_id", "organization_id", "owner_id")},
                    "tenant_id": tenant or str(uuid.uuid4()),
                    "device_id": device or str(uuid.uuid4()), "session_id": session,
                },
            })
            request("/companion/sessions", 204, {"session": session})
            status = request("/companion", 200)
            request("/companion/automation", 204, {
                "action": "start", "expected_session": session,
                "expected_epoch": status["automation"]["epoch"],
            })
            if device:
                bridge = Path(os.environ["HYPERMESH_HOST_BRIDGE_SOCKET"])
                for _ in range(120):
                    if bridge.is_socket():
                        break
                    time.sleep(.15)
                else:
                    raise RuntimeError("The enrolled Host bridge is unavailable")
                settings = Path(USER.pw_dir) / ".vscode-server/data/Machine/settings.json"
                updated = json.loads(settings.read_text())
                updated["hypermesh.terminalHostDeviceId"] = device
                dump(settings, updated)
                wrong = companion("context", device=str(uuid.uuid4()), check=False)
                assert wrong.returncode != 0 and not wrong.stdout, "Foreign device must be refused"
            context = json.loads(companion("context", device=device).stdout)
            assert context["wsl_distro"] == os.environ["WSL_DISTRO_NAME"], context
            assert context["workspace"] == str(WORKSPACE) and context["access"] == "ready", context
            if device:
                assert context["device_id"] == device, context
            run = companion("run", context["task"]["revision"], device=device)
            assert run.stdout.strip() == MARKER, run.stdout
            task_id = re.search(r"^Task ([a-f0-9-]{36})$", run.stderr, re.MULTILINE)[1]
            print(f"CLI ran task {task_id} in {context['wsl_distro']} at {WORKSPACE}", flush=True)
            dump(BASE / "case.json", {"cliTask": task_id, "marker": MARKER, "workspace": str(WORKSPACE), "device": device})
            # Keep the real daemon available for the VS Code window. The Windows
            # job unregisters this distro after the editor and CLI checks.
            while daemon.poll() is None:
                request("/companion", 200)
                time.sleep(0.75)
            raise RuntimeError(f"Visor stopped before the fixture finished ({daemon.returncode})")
        finally:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()


class OwnerHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/machine-context" or not (BASE / "assignment.json").exists():
            self.send_error(404)
            return
        result = (BASE / "assignment.json").read_bytes()
        self.send_response(200)
        self.send_header("Content-Length", str(len(result)))
        self.end_headers()
        self.wfile.write(result)

    def log_message(self, *_):
        pass


def client_environment(owner_socket):
    return {
        "HOME": USER.pw_dir, "USER": USER.pw_name, "PATH": "/usr/bin:/bin",
        "WSL_DISTRO_NAME": os.environ["WSL_DISTRO_NAME"],
        "HYPERMESH_RUNTIME_DIR": str(BASE / "runtime"),
        "HYPERMESH_CONFIG_DIR": str(BASE / "config"),
        "HYPERMESH_HOST_BRIDGE_SOCKET": str(owner_socket),
    }


def owner():
    assert os.getuid() == 0
    with socketserver.UnixStreamServer(str(OWNER), OwnerHandler) as server:
        OWNER.chmod(0o666)
        env = client_environment(OWNER)
        with subprocess.Popen([sys.executable, __file__, "client"], env=env,
                              user=USER.pw_uid, group=USER.pw_gid, extra_groups=[]) as child:
            try:
                server.timeout = 0.25
                while child.poll() is None:
                    server.handle_request()
                raise SystemExit(child.returncode)
            finally:
                if child.poll() is None:
                    child.terminate()


if __name__ == "__main__":
    if sys.argv[1:] == ["client"]:
        client()
    elif len(sys.argv) == 3 and sys.argv[1] == "verify":
        assert os.getuid() == USER.pw_uid
        os.environ["HYPERMESH_RUNTIME_DIR"] = str(BASE / "runtime")
        verify_task(sys.argv[2])
    elif len(sys.argv) == 3 and sys.argv[1] == "verify-revoked":
        assert os.getuid() == USER.pw_uid
        os.environ["HYPERMESH_RUNTIME_DIR"] = str(BASE / "runtime")
        device = json.loads((BASE / "case.json").read_text())["device"]
        refused = companion("watch", str(uuid.UUID(sys.argv[2])), check=False, device=device)
        assert refused.returncode != 0 and refused.stdout == "", "Revoked workspace output must be refused"
        print(f"CLI refused reattachment after revocation for device {device}", flush=True)
    elif not sys.argv[1:]:
        owner()
    else:
        raise SystemExit("Unexpected fixture arguments")
