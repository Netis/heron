#!/usr/bin/env python3
"""Generate LLM-shaped TLS traffic on the staging VM to exercise heron's eBPF
SSL-uprobe capture path, then report what it sent as JSON on stdout.

Runs ENTIRELY on the staging VM (shipped + invoked by ebpf-soak.sh). It:

  1. Stands up a throwaway HTTPS stub on 127.0.0.1 (self-signed cert) that
     answers `POST /v1/chat/completions` with an OpenAI-shaped JSON body — so
     the bytes heron reconstructs parse into a real `LlmCall`.
  2. Drives N requests through `openssl s_client`, NOT curl or Python's `ssl`.
     This is deliberate: heron attaches uprobes to the `SSL_write` / `SSL_read`
     symbols, but CPython's `_ssl` and some curl builds call `SSL_write_ex` /
     `SSL_read_ex` (or link GnuTLS), which those uprobes never see. `openssl
     s_client` is the canonical libssl client and always calls the plain
     `SSL_write` / `SSL_read` on `libssl.so`, so the probe fires deterministically
     and the captured process is attributed to a real pid/comm (`openssl`).

The stub's own TLS termination (Python `ssl`) may use the `_ex` variants and go
unseen — that's fine. The `s_client` connection alone carries both directions
(request via SSL_write, response via SSL_read), which is everything heron needs
to synthesize the flow and emit one attributed LlmCall.

Exit 0 with `{"ok": true, ...}` on stdout if every request got a 200 with the
expected JSON marker; exit 1 with `{"ok": false, ...}` otherwise.
"""

import argparse
import http.server
import json
import os
import ssl
import subprocess
import sys
import tempfile
import threading
import time

# The OpenAI-shaped reply the stub returns. The marker string lets the client
# confirm it round-tripped real plaintext (not a TLS error page).
MARKER = "heron-ebpf-soak"
REPLY_BODY = json.dumps(
    {
        "id": "chatcmpl-ebpfsoak",
        "object": "chat.completion",
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": MARKER},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
    }
).encode()

REQUEST_BODY = json.dumps(
    {
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": "ping from heron ebpf soak"}],
    }
).encode()


class StubHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 (stdlib-mandated name)
        length = int(self.headers.get("Content-Length", "0"))
        if length:
            self.rfile.read(length)
        if self.path == "/v1/chat/completions":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(REPLY_BODY)))
            self.end_headers()
            self.wfile.write(REPLY_BODY)
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()

    def log_message(self, *_args):  # silence per-request stderr noise
        pass


def gen_self_signed(certdir):
    """Make a throwaway self-signed cert with the openssl CLI (always present
    where s_client is). Returns (cert_path, key_path)."""
    cert = os.path.join(certdir, "cert.pem")
    key = os.path.join(certdir, "key.pem")
    subprocess.run(
        [
            "openssl", "req", "-x509", "-newkey", "rsa:2048",
            "-keyout", key, "-out", cert, "-days", "1", "-nodes",
            "-subj", "/CN=localhost",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return cert, key


def start_stub(host, cert, key):
    """Start the HTTPS stub on an ephemeral port; return (server, port)."""
    httpd = http.server.HTTPServer((host, 0), StubHandler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(certfile=cert, keyfile=key)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    port = httpd.socket.getsockname()[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, port


def one_request(host, port):
    """Send one LLM-shaped POST over TLS via `openssl s_client`. Returns the
    decoded response text (empty on failure)."""
    req = (
        "POST /v1/chat/completions HTTP/1.1\r\n"
        "Host: api.openai.com\r\n"
        "Authorization: Bearer sk-heron-ebpf-soak\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(REQUEST_BODY)}\r\n"
        "Connection: close\r\n"
        "\r\n"
    ).encode() + REQUEST_BODY
    try:
        # `-quiet` implicitly enables `-ign_eof`, so s_client keeps reading
        # after our piped request hits stdin EOF and returns only when the
        # server closes the connection (we send `Connection: close`). That's
        # exactly the round-trip we want; do NOT add `-no_ign_eof` (it
        # contradicts that and some libssl builds reject the flag outright).
        proc = subprocess.run(
            ["openssl", "s_client", "-quiet", "-connect", f"{host}:{port}"],
            input=req,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=15,
        )
        return proc.stdout.decode("utf-8", "replace")
    except (subprocess.TimeoutExpired, OSError):
        return ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--requests", type=int, default=8)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory() as certdir:
        cert, key = gen_self_signed(certdir)
        httpd, port = start_stub(args.host, cert, key)
        time.sleep(0.3)  # let the listener settle

        ok_count = 0
        for _ in range(args.requests):
            resp = one_request(args.host, port)
            if MARKER in resp or " 200 " in resp:
                ok_count += 1
            time.sleep(0.2)

        httpd.shutdown()

    ok = ok_count == args.requests and args.requests > 0
    print(json.dumps({
        "ok": ok,
        "requests": args.requests,
        "succeeded": ok_count,
        "port": port,
        "client": "openssl s_client",
        "request_path": "/v1/chat/completions",
    }))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
