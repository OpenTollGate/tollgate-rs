#!/usr/bin/env python3
"""Minimal fake Cashu mint for integration tests.

Implements only NUT-07 check-state: every proof is reported UNSPENT. This lets
the TollGate provider's bootstrap verification run its real code path
(POST /v1/checkstate) without a full mint. It does NOT validate signatures or
track spends — it is a stub for flow testing only.
"""
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import sys


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            body = {}

        if self.path.rstrip("/").endswith("/v1/checkstate"):
            ys = body.get("Ys", [])
            payload = {"states": [{"Y": y, "state": "UNSPENT"} for y in ys]}
            self._send(200, payload)
        else:
            self._send(404, {"error": f"unhandled path {self.path}"})

    def do_GET(self):
        # A liveness endpoint for the container healthcheck.
        if self.path.rstrip("/").endswith("/v1/info"):
            self._send(200, {"name": "fake-mint", "version": "test"})
        else:
            self._send(404, {"error": f"unhandled path {self.path}"})

    def _send(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-mint: " + (fmt % args) + "\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 3338
    print(f"fake-mint listening on :{port}", flush=True)
    HTTPServer(("0.0.0.0", port), Handler).serve_forever()
