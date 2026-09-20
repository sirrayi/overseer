"""Cloud-eval plumbing — relay header injection, shard determinism,
driver matrix shape (zero spend, loopback only)."""

import http.server
import json
import sys
import threading
import urllib.request
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "cloud"))

import driver
import relay
import shard_ids


# ---------- relay ----------


class _Upstream(http.server.BaseHTTPRequestHandler):
    """Captures what the relay forwarded."""

    seen = {}

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n)
        _Upstream.seen = {
            "path": self.path,
            "session": self.headers.get("x-opencode-session"),
            "auth": self.headers.get("authorization"),
            "body": body,
        }
        payload = b'{"ok":true,"echo":' + json.dumps(body.decode()) .encode() + b"}"
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        pass


@pytest.fixture
def upstream():
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Upstream)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    yield srv
    srv.shutdown()


@pytest.fixture
def relay_server(upstream, monkeypatch):
    port_up = upstream.server_address[1]
    monkeypatch.setattr(relay, "UPSTREAM",
                        f"http://127.0.0.1:{port_up}/zen/go/v1")
    monkeypatch.setattr(relay, "SESSION_TAG", "test-session-1")
    monkeypatch.setattr(relay, "KEY", "sk-relay-test")
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), relay.Relay)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    yield srv
    srv.shutdown()


class TestRelay:
    def test_injects_session_and_auth(self, relay_server):
        port = relay_server.server_address[1]
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/v1/chat/completions",
            data=b'{"model":"muse-spark-1.3-contributor"}',
            headers={"content-type": "application/json"},
        )
        resp = json.loads(urllib.request.urlopen(req, timeout=10).read())
        assert resp["ok"] is True
        # /v1 stripped locally, upstream base path prepended
        assert _Upstream.seen["path"] == "/zen/go/v1/chat/completions"
        assert _Upstream.seen["session"] == "test-session-1"
        assert _Upstream.seen["auth"] == "Bearer sk-relay-test"

    def test_client_auth_wins(self, relay_server):
        port = relay_server.server_address[1]
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/v1/models",
            data=b"{}",
            headers={"authorization": "Bearer client-key"},
        )
        urllib.request.urlopen(req, timeout=10)
        assert _Upstream.seen["auth"] == "Bearer client-key"
        assert _Upstream.seen["session"] == "test-session-1"


# ---------- shard_ids slicing (dataset load stubbed) ----------


class TestShards:
    IDS = [f"inst-{i:03d}" for i in range(100)]

    def _slice(self, ids, shard, of):
        n = len(ids)
        size = (n + of - 1) // of
        return ids[shard * size:(shard + 1) * size]

    def test_disjoint_and_complete(self):
        parts = [self._slice(self.IDS, i, 12) for i in range(12)]
        flat = [x for p in parts for x in p]
        assert flat == self.IDS
        assert len(set(flat)) == 100

    def test_deterministic(self):
        a = self._slice(self.IDS, 3, 12)
        b = self._slice(self.IDS, 3, 12)
        assert a == b == ["inst-027", "inst-028", "inst-029",
                          "inst-030", "inst-031", "inst-032",
                          "inst-033", "inst-034", "inst-035"]


# ---------- driver matrix ----------


class TestDriverMatrix:
    def test_matrix_shape(self):
        specs = driver.matrix()
        runs = [s["run"] for s in specs]
        assert len(runs) == len(set(runs)), "run names must be unique"
        assert "stress-muse" in runs
        models = {s["model"] for s in specs}
        assert models == {"muse-spark-1.3-contributor",
                          "deepseek-v4.1-flash"}
        # 12 sweb shards per model, disjoint shard indices
        for tag in ("sweb-muse-s", "sweb-deepseek-s"):
            idxs = sorted(int(r.rsplit("s", 1)[1]) for r in runs
                          if r.startswith(tag))
            assert idxs == list(range(12))
        for s in specs:
            assert s["commands"], f"{s['run']} has no commands"

    def test_sweb_commands_shard_ids(self):
        spec = next(s for s in driver.matrix() if s["run"] == "sweb-muse-s00")
        cmd = spec["commands"][0]
        assert "shard_ids.py" in cmd and "--shard 0 --of 12" in cmd
        assert "--swebench-ids" in cmd

    def test_prompt_keeps_secret_out(self):
        spec = next(s for s in driver.matrix() if s["run"] == "stress-muse")
        prompt = driver.worker_prompt(spec)
        assert "sk-" not in prompt  # no literal key material
        assert "OPENCODE_API_KEY" in prompt  # env name referenced, not value
        assert "RUN_RESULT" in prompt

    def test_launch_dedup(self, tmp_path, monkeypatch):
        monkeypatch.setattr(driver, "STATE", tmp_path / "state.json")
        driver.save_state({"sessions": {
            "sess-1": {"run": "stress-muse", "ts": 0}}})
        st = driver.load_state()
        launched = {v["run"] for v in st["sessions"].values()}
        assert "stress-muse" in launched
        assert "sweb-muse-s00" not in launched
