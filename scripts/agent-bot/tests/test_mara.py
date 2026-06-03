#!/usr/bin/env python3
"""Integration test for mara.sh's confirm-debounce (option A) — stdlib only, so
CI can run it without installing anything:

    python3 scripts/agent-bot/tests/test_mara.py

Spins up a stub `/api/health` server whose responses follow a per-test script
(one entry per request; the last entry repeats once the script is exhausted),
then runs mara.sh in DRY_RUN with CONFIRM_DELAY=0 and asserts whether it would
file. The point under test: a single failed poll that recovers on a later poll
(a deploy/restart blip) must NOT open an incident; only an all-polls-failed
run does.

Exit 0 = all pass, 1 = a failure.
"""
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
MARA = os.path.join(os.path.dirname(HERE), "mara.sh")

# Response script the handler walks through. Each entry is one of:
#   {"code": 503}                      → down (non-200)
#   {"code": 200, "running": True}     → healthy
#   {"code": 200, "running": False}    → parked
#   {"code": 200, "body": "<garbage>"} → unparseable 200
_script = []
_reqs = [0]
_lock = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with _lock:
            i = _reqs[0]
            _reqs[0] += 1
            spec = _script[min(i, len(_script) - 1)] if _script else {"code": 200, "running": True}
        code = spec.get("code", 200)
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        if code == 200:
            if "body" in spec:
                payload = spec["body"]
            else:
                payload = json.dumps(
                    {"data": {"pipelines": [{"running": spec.get("running", True)}]}}
                )
            self.wfile.write(payload.encode())

    def log_message(self, *_a):  # silence per-request logging
        pass


def _set_script(script):
    with _lock:
        _script[:] = script
        _reqs[0] = 0


def _run_mara(script, **env_over):
    """Set the stub script, run mara.sh (DRY_RUN), return combined output."""
    _set_script(script)
    tmp = tempfile.mkdtemp(prefix="mara-test-")
    env = dict(os.environ)
    env.update(
        {
            "MARA_HEALTH_URL": f"http://127.0.0.1:{PORT}/api/health",
            "MARA_DRY_RUN": "1",
            "MARA_CONFIRM_DELAY_SECS": "0",
            "MARA_CONFIRM_POLLS": "3",
            "MARA_STATE_DIR": tmp,
            "MARA_LOG_HOST": "",  # no ssh log-context fetch
        }
    )
    env.update(env_over)
    r = subprocess.run(
        ["bash", MARA], capture_output=True, text=True, env=env, timeout=30
    )
    return r.stdout + r.stderr


# ---- tests ----------------------------------------------------------------

def test_healthy_no_file():
    out = _run_mara([{"code": 200, "running": True}])
    assert "prod heron OK" in out, out
    assert "would file" not in out, out


def test_transient_down_then_up_does_not_file():
    # poll1 = down (a restart blip), poll2 = healthy → transient, no incident.
    out = _run_mara([{"code": 503}, {"code": 200, "running": True}])
    assert "transient" in out and "not filing" in out, out
    assert "would file" not in out, out


def test_sustained_down_files():
    # every poll fails → real outage → would file prod-heron-down.
    out = _run_mara([{"code": 503}])
    assert "would file" in out, out
    assert "prod-heron-down" in out, out
    assert "3/3 consecutive polls" in out, out


def test_transient_parked_then_running_does_not_file():
    # pipeline briefly running=false during deploy, then resumes → no incident.
    out = _run_mara([{"code": 200, "running": False}, {"code": 200, "running": True}])
    assert "transient" in out and "not filing" in out, out
    assert "would file" not in out, out


def test_sustained_parked_files():
    out = _run_mara([{"code": 200, "running": False}])
    assert "would file" in out, out
    assert "prod-heron-parked" in out, out


def test_confirm_polls_1_files_on_first_failure():
    # CONFIRM_POLLS=1 disables debounce → first failure files immediately.
    out = _run_mara([{"code": 503}, {"code": 200, "running": True}], MARA_CONFIRM_POLLS="1")
    assert "would file" in out, out
    assert "prod-heron-down" in out, out


def main():
    global PORT
    srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    PORT = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    fails = 0
    for t in tests:
        try:
            t()
            print(f"ok   {t.__name__}")
        except AssertionError as e:
            fails += 1
            print(f"FAIL {t.__name__}: {e}")
        except Exception as e:  # noqa: BLE001
            fails += 1
            print(f"ERR  {t.__name__}: {type(e).__name__}: {e}")
    srv.shutdown()
    print(f"\n{len(tests) - fails}/{len(tests)} passed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
