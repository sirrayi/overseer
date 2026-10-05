#!/usr/bin/env python3
"""eval/cloud/relay.py — localhost OpenAI-compat relay injecting
`x-opencode-session`.

Harbor's litellm-based agents (and any other stock OpenAI client) can't
attach opencode Go's required routing header. Point them at
`http://127.0.0.1:PORT/v1` and this forwards to the real base with the
header added — plus Authorization from OPENCODE_API_KEY if the client
sent none. Streaming passes through unbuffered.

  OPENCODE_API_KEY=sk-… uv run python relay.py --port 8399 &
  OPENAI_BASE_URL=http://127.0.0.1:8399/v1 OPENAI_API_KEY=x harbor run …

stdlib-only. Single-process ThreadingHTTPServer — fine for one agent.

DEFERRED(relay): the listener trusts anything on 127.0.0.1 — no auth on
the local side. Acceptable for a benchmark worker VM (same trust
boundary as the agent itself); do NOT expose it beyond loopback.
"""

from __future__ import annotations

import argparse
import http.client
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

UPSTREAM = os.environ.get("RELAY_UPSTREAM", "https://opencode.ai/zen/go/v1")
SESSION_TAG = os.environ.get("RELAY_SESSION", "overseer-relay")
KEY = os.environ.get("OPENCODE_API_KEY", "")


def _conn(up) -> http.client.HTTPConnection:
    port = up.port or (443 if up.scheme == "https" else 80)
    cls = (http.client.HTTPSConnection if up.scheme == "https"
           else http.client.HTTPConnection)
    return cls(up.hostname, port, timeout=600)


class Relay(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):  # quiet per-request logging
        sys.stderr.write(f"[relay] {fmt % a}\n")

    def _proxy(self):
        up = urlsplit(UPSTREAM)
        path = self.path
        # strip the local /v1 prefix — upstream already carries it
        if path.startswith("/v1"):
            path = path[len("/v1"):]
        base = urlsplit(UPSTREAM).path.rstrip("/")
        path = base + path
        conn = _conn(up)
        body = None
        if self.command in ("POST", "PUT", "PATCH"):
            n = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(n) if n else None
        headers = {
            k: v for k, v in self.headers.items()
            if k.lower() not in ("host", "content-length", "connection")
        }
        headers["x-opencode-session"] = SESSION_TAG
        if KEY and "authorization" not in {k.lower() for k in headers}:
            headers["authorization"] = f"Bearer {KEY}"
        conn.request(self.command, path, body=body, headers=headers)
        resp = conn.getresponse()
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() not in ("transfer-encoding", "connection"):
                self.send_header(k, v)
        self.send_header("connection", "close")
        self.end_headers()
        while True:
            chunk = resp.read1(65536)  # returns as data arrives — SSE stays live
            if not chunk:
                break
            self.wfile.write(chunk)
            self.wfile.flush()
        conn.close()

    do_GET = _proxy
    do_POST = _proxy
    do_PUT = _proxy
    do_DELETE = _proxy


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8399)
    args = ap.parse_args()
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Relay)
    sys.stderr.write(f"[relay] :{args.port} → {UPSTREAM}\n")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
