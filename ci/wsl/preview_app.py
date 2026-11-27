"""Project-owned development app for the installed preview contract."""
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = 4173
TITLE = "Visor Workspace Preview Fixture"


class App(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/":
            self.send_error(404)
            return
        body = b"<!doctype html><title>Visor Workspace Preview Fixture</title><h1>Visor Workspace Preview Fixture</h1>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    with HTTPServer(("127.0.0.1", PORT), App) as app:
        app.serve_forever()
