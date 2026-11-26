"""Check the Linux command against a same-user fixture socket."""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile


binary = Path(__file__).resolve().parents[2] / "target/debug/hypermesh-companion"
fixture = b'{"x":1,\n "x":2}\n'
digest = hashlib.sha256(fixture).hexdigest()
report = {
    "file": "sample.json",
    "content_sha256": digest,
    "display_sha256": digest,
    "runtime": "local-stdio",
    "linter_version": "0.1.0",
    "config": "json-strict-v1",
    "truncated": False,
    "diagnostics": [{
        "line": 2,
        "column": 2,
        "rule": "duplicate-key",
        "severity": "warning",
        "message": "Duplicate JSON object key",
    }],
}

with tempfile.TemporaryDirectory(prefix="visor-lint-") as tmp:
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(Path(tmp) / "companion.sock"))
        listener.listen()
        listener.settimeout(5)
        for status, response, code, expected in (
            (200, {"result": report}, 1, {"result": report}),
            (200, {"result": {**report, "diagnostics": []}}, 0,
             {"result": {**report, "diagnostics": []}}),
            (403, {"error": "Workspace access refused"}, 2,
             {"error": {"message": "Workspace access refused"}}),
        ):
            with subprocess.Popen(
                [binary, "--workspace-lint", tmp, "sample.json"],
                env={**os.environ, "HYPERMESH_RUNTIME_DIR": tmp},
                stdout=subprocess.PIPE,
            ) as child:
                try:
                    stream, _ = listener.accept()
                    with stream, stream.makefile("rb") as request:
                        stream.settimeout(5)
                        assert request.readline() == b"POST /workspace-lint HTTP/1.0\r\n"
                        headers = {}
                        while line := request.readline().strip():
                            key, _, value = line.partition(b":")
                            headers[key.lower()] = value.strip()
                        payload = request.read(int(headers[b"content-length"]))
                        assert json.loads(payload) == {"workspace": str(Path(tmp).resolve()), "file": "sample.json"}
                        body = json.dumps(response).encode()
                        stream.sendall(f"HTTP/1.0 {status} Fixture\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
                    output, _ = child.communicate(timeout=5)
                    assert child.returncode == code
                    assert json.loads(output) == expected
                finally:
                    if child.poll() is None:
                        child.kill()

print("Workspace lint CLI contract passed")
