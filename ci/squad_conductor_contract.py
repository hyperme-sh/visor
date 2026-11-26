"""Packaged Conductor and Visor CLI contract for the two partner-pilot roles."""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler
import json
import os
import socketserver
import subprocess
from threading import Thread
from uuid import uuid4


class ControlFixture(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/squad/publish":
            self.send_error(404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if request["planeTaskId"] != self.server.plane_task:
            self.send_error(403)
            return
        artifact = request["artifact"]
        draft = artifact["draft"]
        if draft["taskId"] != self.server.squad_task:
            self.send_error(403)
            return
        key = (self.server.plane_task, artifact["kind"])
        prior = self.server.revisions.setdefault(key, artifact)
        if prior != artifact:
            self.send_error(409)
            return
        reply = {"version": 1, **{name: draft[name] for name in ("taskId", "artifactId", "revisionId")}}
        data = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


@contextmanager
def control(runtime, plane_task, squad_task, revisions):
    runtime.mkdir(mode=0o700)
    socket = runtime / "control.sock"
    with socketserver.UnixStreamServer(str(socket), ControlFixture) as server:
        socket.chmod(0o600)
        server.plane_task = plane_task
        server.squad_task = squad_task
        server.revisions = revisions
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield
        finally:
            server.shutdown()
            thread.join()


def publish(visor, runtime, plane_task, artifact, expected_status=0):
    result = subprocess.run(
        [visor, "squad-publish", plane_task],
        input=json.dumps(artifact), encoding="utf-8", capture_output=True, timeout=18,
        env={**os.environ, "HYPERMESH_RUNTIME_DIR": str(runtime)},
    )
    assert result.returncode == expected_status, result.stderr
    if expected_status:
        assert not result.stdout
        return result.stderr
    return json.loads(result.stdout)


def generate(conductor, config, journal, provider, plane_task, prompt, scripted):
    message = json.dumps(prompt, sort_keys=True)
    provider.scripted = {message: json.dumps(scripted)}
    step = str(uuid4())
    result = subprocess.run(
        [conductor, "run", "--config", config, "--journal", journal, "--task", plane_task,
         "--step", step, "--class", "code", "--message", message],
        encoding="utf-8", capture_output=True, check=True, timeout=15,
    )
    model = json.loads(result.stdout)
    assert (model["task_id"], model["step_id"], model["model_id"]) == (plane_task, step, "coding")
    return json.loads(model["content"])


def artifact(kind, task, **draft):
    return {"kind": kind, "draft": {
        "taskId": task, "artifactId": str(uuid4()), "revisionId": str(uuid4()), **draft,
    }}


def quote_cents(inputs):
    return inputs["quantity"] * inputs["unitPriceCents"] * (10000 - inputs["discountBps"]) // 10000


def check_partner_handoff(visor, conductor, folder, config, provider):
    task, desktop, headless = (str(uuid4()) for _ in range(3))
    desktop_run = folder / "partner-desktop"
    headless_run = folder / "partner-headless"
    journal = folder / "partner.jsonl"
    revisions = {}
    pricing = {"quantity": 10, "unitPriceCents": 12000, "discountBps": 0}
    copy = {"title": "Partner pilot", "summary": "A 30-day pilot for 10 seats",
            "highlights": [], "nextStep": "Confirm the quote"}
    calls = len(provider.calls)
    draft = generate(conductor, config, journal, provider, desktop,
                     {"role": "desktop", "squadTaskId": task, "seats": 10, "days": 30},
                     {"markdown": "A 30-day pilot for 10 seats", "inputs": pricing, "copy": copy})
    assert "30-day" in draft["markdown"] and "10 seats" in draft["markdown"]
    assert quote_cents(draft["inputs"]) == 120000
    with control(desktop_run, desktop, task, revisions):
        brief = publish(visor, desktop_run, desktop, artifact("brief", task, base=None, markdown=draft["markdown"]))
        workbook = publish(visor, desktop_run, desktop, artifact("workbook", task, inputs=draft["inputs"]))
        initial = artifact("preview", task, base=None, briefRevisionId=brief["revisionId"],
                           workbookRevisionId=workbook["revisionId"], copy=draft["copy"])
        preview = publish(visor, desktop_run, desktop, initial)

    edited = {**pricing, "discountBps": 1000}
    edited_workbook = str(uuid4())
    assert quote_cents(edited) == 108000
    checkpoint = {"role": "headless", "squadTaskId": task, "briefRevisionId": brief["revisionId"],
                  "workbookRevisionId": edited_workbook, "previewRevisionId": preview["revisionId"],
                  "sheet": "Pricing", "cell": "B5", "quoteCents": quote_cents(edited)}
    successor_copy = {**copy, "summary": "A 30-day pilot for 10 seats with the approved 10% discount"}
    next_copy = generate(conductor, config, journal, provider, headless, checkpoint, successor_copy)
    revised = artifact("preview", task, artifactId=preview["artifactId"], base=preview["revisionId"],
                       briefRevisionId=brief["revisionId"], workbookRevisionId=edited_workbook, copy=next_copy)
    with control(headless_run, headless, task, revisions):
        assert "current completed Squad delivery" in publish(visor, headless_run, desktop, revised, expected_status=1)
        update = publish(visor, headless_run, headless, revised)
        assert publish(visor, headless_run, headless, revised) == update
        changed = {**revised, "draft": {**revised["draft"], "workbookRevisionId": workbook["revisionId"]}}
        assert "Squad artifact was refused" in publish(visor, headless_run, headless, changed, expected_status=1)
    assert update["taskId"] == preview["taskId"] == task
    assert update["artifactId"] == preview["artifactId"] and update["revisionId"] != preview["revisionId"]
    assert revisions[(desktop, "preview")]["draft"]["workbookRevisionId"] == workbook["revisionId"]
    assert revisions[(headless, "preview")]["draft"]["workbookRevisionId"] == edited_workbook
    assert len(revisions) == 4 and len(provider.calls) == calls + 2
    print(f"partner contract task={task} desktop={desktop} headless={headless} "
          f"preview={preview['revisionId']} revised={update['revisionId']} Pricing!B5=120000,108000", flush=True)
