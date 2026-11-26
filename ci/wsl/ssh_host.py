"""CI-only enrolled Host and SSH fixture within the disposable WSL machine."""
import base64
import contextlib
import hashlib
import hmac
import http.server
import json
import os
from pathlib import Path
import secrets
import ssl
import subprocess
import sys
import threading
import time
from uuid import UUID, uuid4

import task

STATE = Path("/var/lib/visor-ssh-ci")
BRIDGE = Path("/run/visor-ssh-ci/owner.sock")
SSH_PORT = 22222
API = "/api/v1/hypermesh"


def private_json(path, document):
    with open(path, "x", opener=lambda name, flags: os.open(name, flags, 0o600)) as output:
        json.dump(document, output)
        output.write("\n")


def run(*args, **options):
    return subprocess.run(args, check=True, timeout=15, **options)


def wait_for(label, process, ready, seconds=35):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"{label} exited ({process.returncode})")
        if ready():
            return
        time.sleep(.15)
    raise RuntimeError(f"{label} did not become ready")


@contextlib.contextmanager
def child(*args, **options):
    with subprocess.Popen(args, **options) as process:
        try:
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()


class Authority:
    def __init__(self):
        self.tenant, self.device = str(uuid4()), str(uuid4())
        self.enroll = secrets.token_hex(32)
        self.token = f"hm_msi_{self.device}.{secrets.token_urlsafe(32)}"
        self.verifier = ""
        self.public_key = ""
        self.assignment = None
        self.authority_id = "visor-ci-authority"
        self.plane_id = "visor-ci-plane"
        self.nonces = set()

    def verify_proof(self, request):
        if (request.get("protocol_version") != 1 or request.get("device_id") != self.device
                or request.get("request_id") != self.device
                or request.get("authority_id") != self.authority_id
                or request.get("control_plane_id") != self.plane_id
                or request.get("next_public_key") != ""
                or abs(int(time.time()) - request.get("issued_at", 0)) > 60):
            return False
        nonce = request.get("nonce")
        if not isinstance(nonce, str) or len(nonce) != 64 or nonce in self.nonces:
            return False
        public = base64.b64decode(request["public_key"], validate=True)
        signature = base64.b64decode(request["signature"], validate=True)
        if len(public) != 32 or len(signature) != 64:
            return False
        message = "\n".join((
            "hypermesh-machine-identity-v1", self.tenant, self.device, self.device,
            self.authority_id, self.plane_id, request["public_key"],
            str(request["issued_at"]), nonce, "",
        ))
        # SubjectPublicKeyInfo DER prefix for Ed25519.
        (STATE / "proof.pub.der").write_bytes(bytes.fromhex("302a300506032b6570032100") + public)
        (STATE / "proof.sig").write_bytes(signature)
        (STATE / "proof.message").write_text(message, encoding="utf-8")
        run("openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
            "-inkey", STATE / "proof.pub.der", "-sigfile", STATE / "proof.sig",
            "-in", STATE / "proof.message", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.nonces.add(nonce)
        return True

    def config(self):
        return {"authority_id": self.authority_id, "control_plane_id": self.plane_id}

    def route(self, method, path, request, authorization):
        if method == "POST" and path == f"{API}/agent/enroll/{self.device}":
            if request.get("enroll_token") != self.enroll or request.get("host_name") != "visor-ssh-fixture":
                return 403, None
            digest = request.get("device_secret_sha256")
            if not isinstance(digest, str) or len(digest) != 64:
                return 400, None
            self.verifier = digest
            return 204, None
        if method == "GET" and path == f"{API}/agent/identity/configuration":
            return (200, self.config()) if self.verifier else (403, None)
        if method == "POST" and path in {
            f"{API}/agent/identity/bind", "/api/v1/aiuditor/machine/renew",
        }:
            if path.endswith("/bind"):
                secret = authorization.removeprefix("Bearer ")
                if not self.verifier or not hmac.compare_digest(
                    self.verifier, hashlib.sha256(secret.encode()).hexdigest()
                ):
                    return 401, None
            elif self.public_key != request.get("public_key"):
                return 401, None
            if not self.verify_proof(request):
                return 403, None
            self.public_key = request["public_key"]
            return 200, {
                "access_token": self.token, "token_type": "Bearer", "expires_in": 600,
                "public_key": self.public_key, "key_created_at": int(time.time()),
            }
        if not self.public_key or not hmac.compare_digest(authorization, "Bearer " + self.token):
            return 401, None
        if path in {f"{API}/agent/tunnel", f"{API}/agent/jobs"}:
            return 204, None
        if path == f"{API}/agent/heartbeat":
            return 200, {}
        fact = self.assignment
        if fact and method == "POST" and path == f"{API}/bots/{fact['bot_id']}/machine/context":
            if (STATE / "revoked").exists():
                return 403, None
            for key in ("device_id", "assignment_id", "session_id"):
                if request.get(key) != fact[key]:
                    return 403, None
            request_id = str(UUID(request["request_id"]))
            return 200, {**fact, "version": 1, "request_id": request_id,
                         "principal": "visor-ci-owner", "expires_at": time.time() + 8}
        return 404, None


class PlaneHandler(http.server.BaseHTTPRequestHandler):
    def dispatch(self):
        authority = self.server.authority
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("X-Tenant-ID") != authority.tenant:
                status, value = 403, None
            elif not 0 <= size <= 65536:
                status, value = 413, None
            else:
                request = json.loads(self.rfile.read(size)) if size else {}
                status, value = authority.route(
                    self.command, self.path.partition("?")[0], request, self.headers.get("Authorization", ""),
                )
        except (ValueError, KeyError, TypeError, subprocess.SubprocessError):
            status, value = 400, None
        data = json.dumps(value).encode() if value is not None else b""
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data:
            self.wfile.write(data)

    do_GET = dispatch
    do_POST = dispatch

    def log_message(self, *_):
        pass


@contextlib.contextmanager
def authority_server(authority):
    ca = Path("/usr/local/share/ca-certificates/visor-ssh-ci.crt")
    key = STATE / "plane.key"
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        "-subj", "/CN=visor-ssh-ci", "-addext", "subjectAltName=IP:127.0.0.1",
        "-out", ca, "-keyout", key, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    run("update-ca-certificates", stdout=subprocess.DEVNULL)
    with http.server.HTTPServer(("127.0.0.1", 0), PlaneHandler) as server:
        server.authority = authority
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(ca, key)
        server.socket = tls.wrap_socket(server.socket, server_side=True)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"https://127.0.0.1:{server.server_port}"
        finally:
            server.shutdown()
            thread.join(timeout=5)


def ssh_config(public_key):
    key = base64.b64decode(public_key, validate=True)
    ed25519_prefix = b"\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20"
    if len(key) != len(ed25519_prefix) + 32 or not key.startswith(ed25519_prefix):
        raise RuntimeError("An Ed25519 fixture client key is required")
    home = Path(task.USER.pw_dir)
    authorized = home / ".ssh/authorized_keys"
    authorized.parent.mkdir(mode=0o700)
    encoded_key = base64.b64encode(key).decode("ascii")
    authorized.write_text(f"ssh-ed25519 {encoded_key}\n", encoding="utf-8")
    authorized.chmod(0o600)
    for path in (authorized.parent, authorized):
        os.chown(path, task.USER.pw_uid, task.USER.pw_gid)
    host_key = STATE / "ssh_host_ed25519"
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", host_key)
    Path("/run/sshd").mkdir(exist_ok=True)
    distro = os.environ["WSL_DISTRO_NAME"]
    if not distro.startswith("VisorCI-") or not distro.replace("-", "").isalnum():
        raise RuntimeError("The disposable CI distribution is required")
    config = STATE / "sshd_config"
    config.write_text(
        f"Port {SSH_PORT}\nListenAddress 127.0.0.1\nHostKey {host_key}\n"
        "UsePAM yes\nPermitRootLogin no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        f"AllowUsers {task.USER.pw_name}\nAllowTcpForwarding local\nGatewayPorts no\nX11Forwarding no\n"
        f"SetEnv WSL_DISTRO_NAME={distro} HYPERMESH_RUNTIME_DIR={task.BASE / 'runtime'}\n",
        encoding="utf-8",
    )
    return config


def main(public_key):
    if os.geteuid() != 0 or not os.environ.get("WSL_DISTRO_NAME", "").startswith("VisorCI-"):
        raise RuntimeError("Run only inside the disposable CI distribution")
    os.umask(0o077)
    STATE.mkdir()
    BRIDGE.parent.mkdir(mode=0o755)
    BRIDGE.parent.chmod(0o755)
    host_state = STATE / "host"
    host_state.mkdir()
    authority = Authority()
    with authority_server(authority) as origin, child(
        task.INSTALL / "host-agent",
        env={
            "HOME": str(STATE), "PATH": "/usr/bin:/bin", "HYPERMESH_STATE_DIR": str(host_state),
            "HYPERMESH_CONTROL_PLANE_URL": origin, "HYPERMESH_TENANT_ID": authority.tenant,
            "HYPERMESH_ENROLL_TOKEN": authority.enroll, "HYPERMESH_MANAGED_DEVICE_ID": authority.device,
            "HYPERMESH_HOST_NAME": "visor-ssh-fixture", "HYPERMESH_WG_IFACE": "visorci-ssh",
            "HYPERMESH_GGUF_PATH": str(STATE / "models/fixture.gguf"), "HYPERMESH_LICENSE_ACCEPT": "yes",
        },
    ) as agent:
        identity_file = host_state / "machine-identity.json"
        wait_for("Host enrollment and signing identity", agent, lambda: authority.public_key and identity_file.is_file())
        record = json.loads((host_state / "device.json").read_text())
        identity = json.loads(identity_file.read_text())
        if record["device_id"] != authority.device or not identity.get("bound") or identity["device"] != authority.device:
            raise RuntimeError("First-boot identity does not match the fixture authority")
        print(f"Enrolled Host device {authority.device} through the managed enrollment path", flush=True)

        environment = task.client_environment(BRIDGE)
        environment["VISOR_SSH_FIXTURE_DEVICE"] = authority.device
        environment["VISOR_SSH_FIXTURE_TENANT"] = authority.tenant
        with child(sys.executable, task.__file__, "client", env=environment,
                   user=task.USER.pw_uid, group=task.USER.pw_gid, extra_groups=[]) as client:
            assignment = task.BASE / "assignment.json"
            wait_for("Visor local session", client, assignment.is_file)
            candidate = json.loads(assignment.read_text())
            if (candidate["workspace"] != str(task.WORKSPACE)
                    or candidate["identity"]["tenant_id"] != authority.tenant
                    or candidate["identity"]["device_id"] != authority.device):
                raise RuntimeError("The Visor workspace does not match the enrolled host")
            authority.assignment = candidate["identity"]
            dispatch = STATE / "dispatch.json"
            private_json(dispatch, {})
            config = STATE / "bridge.json"
            private_json(config, {
                "schema": 1, "client_uid": task.USER.pw_uid, "socket": str(BRIDGE),
                "conductor": str(task.INSTALL / "conductor"), "dispatch_config": str(dispatch),
                "authority_url": origin + "/api/v1/aiuditor/capabilities/review", "base": origin,
                "tenant": authority.tenant, "device": authority.device, "identity": str(identity_file),
                "enabled": [],
                "machine_prompt": {
                    **{name: authority.assignment[name] for name in
                       ("bot_id", "assignment_id", "session_id", "organization_id")},
                    "workspace": str(task.WORKSPACE), "socket": str(STATE / "conductor.sock"),
                    "class": "coding", "destination": "fixture", "max_output_tokens": 64,
                },
            })
            with child(task.INSTALL / "host-cli", "visor", "bridge", "--config", config) as bridge:
                wait_for("The root-owned Host bridge", bridge, BRIDGE.is_socket)
                config = ssh_config(public_key)
                with child("/usr/sbin/sshd", "-D", "-e", "-f", config) as sshd:
                    while all(p.poll() is None for p in (agent, bridge, client, sshd)):
                        time.sleep(.25)
                    raise RuntimeError("An enrolled SSH fixture process stopped early")


if __name__ == "__main__":
    main(sys.argv[1])
