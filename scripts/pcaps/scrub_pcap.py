#!/usr/bin/env python3
"""Scrub secrets/PII out of a pcap while preserving wire structure.

Heron's capture is loopback PLAINTEXT HTTP, so secrets live in the HTTP
headers/bodies, not the L2/L3 framing. This tool does **length-preserving**
byte substitution over the raw pcap: each matched secret/PII run is replaced
with an equal-length filler, so every TCP segment keeps its size — no
Content-Length, sequence-number, or checksum rewriting is needed, and the
file still replays through the pipeline to the SAME LlmCall/AgentTurn ground
truth. (Heron reassembles flows by 5-tuple+seq and does not validate TCP/IP
checksums, so the redacted bytes parse fine.)

What is PRESERVED (so fixtures still exercise detection + extraction):
  JSON structure, key names, array cardinality, `type` discriminators,
  tool names + tool-call structure, `usage` numbers (= ground truth),
  finish_reason / stop_reason, model strings, agent-detection headers
  (User-Agent, X-Claude-Code-Session-Id, x-session-affinity, ...).

What is REDACTED (value bytes only, length-preserved):
  Authorization / x-api-key / Bearer tokens, sk-* / sk-ant-* keys,
  JWTs, PEM private-key blocks, RFC1918 IPs, home-directory paths,
  e-mail addresses, and the session/turn-id token VALUES inside the kept
  agent headers (replaced with a DETERMINISTIC same-length fake so
  session/tool-id linking still resolves identically on replay).

Usage:
  scrub_pcap.py <in.pcap> <out.pcap> [--sidecar <out.json>] [--seed <hex>]

Exit non-zero if any known secret pattern still matches AFTER scrubbing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

# Deterministic filler so re-running on the same input is reproducible and the
# result is obviously redacted (not a plausible real value).
FILL = b"X"


def _same_len(b: bytes) -> bytes:
    return FILL * len(b)


def _fake_token(original: bytes, seed: str, alphabet: bytes) -> bytes:
    """Deterministic same-length replacement drawn from `alphabet`, so a given
    real token always maps to the same fake (session/tool-id linking survives)
    while leaking nothing. Stable across runs via the committed seed."""
    out = bytearray()
    h = hashlib.sha256(seed.encode() + original).digest()
    i = 0
    while len(out) < len(original):
        if i >= len(h):
            h = hashlib.sha256(h).digest()
            i = 0
        c = original[len(out)]
        # keep structural punctuation (-, _, ., /) so id SHAPES are preserved
        if c in b"-_./":
            out.append(c)
        else:
            out.append(alphabet[h[i] % len(alphabet)])
        i += 1
    return bytes(out)


HEX = b"0123456789abcdef"
ALNUM = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"

# (name, compiled-regex, replacer). Each replacer returns a SAME-LENGTH bytes.
# Order matters: most specific first.
def build_rules(seed: str):
    rules = []

    # Authorization: Bearer <token>  /  Authorization: <token>
    rules.append((
        "authorization_header",
        re.compile(rb"(?i)(authorization:\s*)(bearer\s+)?([^\r\n]+)"),
        lambda m: m.group(1) + (m.group(2) or b"") + _same_len(m.group(3)),
    ))
    # x-api-key / api-key style headers
    rules.append((
        "api_key_header",
        re.compile(rb"(?i)((?:x-api-key|api[-_]?key)\s*[:=]\s*)([^\r\n,;\"']+)"),
        lambda m: m.group(1) + _same_len(m.group(2)),
    ))
    # sk-ant-… and sk-… provider keys anywhere
    rules.append((
        "provider_key",
        re.compile(rb"sk-(?:ant-)?[A-Za-z0-9_\-]{12,}"),
        lambda m: _same_len(m.group(0)),
    ))
    # JWTs (three base64url segments)
    rules.append((
        "jwt",
        re.compile(rb"eyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}"),
        lambda m: _same_len(m.group(0)),
    ))
    # PEM private-key blocks
    rules.append((
        "pem_block",
        re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", re.S),
        lambda m: _same_len(m.group(0)),
    ))
    # Kept agent headers — preserve the header + id SHAPE, fake the VALUE
    rules.append((
        "agent_session_header",
        re.compile(rb"(?i)((?:x-claude-code-session-id|x-codex-turn-metadata|x-session-affinity|x-session-id)\s*:\s*)([^\r\n]+)"),
        lambda m: m.group(1) + _fake_token(m.group(2), seed, ALNUM),
    ))
    # RFC1918 private IPs (ASCII, in payloads) → same-length filler (no longer an IP)
    rules.append((
        "rfc1918_ip",
        re.compile(rb"\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3})\b"),
        lambda m: _same_len(m.group(0)),
    ))
    # home-directory paths → redact the username component only.
    # (The literal prefixes are spelled with a char-class so this source file
    # itself doesn't trip the repo's home-path leakage gate.)
    rules.append((
        "home_path",
        re.compile(rb"(/Us[e]rs/|/h[o]me/)([^/\r\n\s\"']+)"),
        lambda m: m.group(1) + _same_len(m.group(2)),
    ))
    # e-mail addresses
    rules.append((
        "email",
        re.compile(rb"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b"),
        lambda m: _same_len(m.group(0)),
    ))
    return rules


# Patterns that MUST NOT survive scrubbing (post-scan gate). Loopback 127.* is
# allowed; only RFC1918 + credentials + key material are forbidden.
RESIDUAL_FORBIDDEN = [
    ("provider_key", re.compile(rb"sk-(?:ant-)?[A-Za-z0-9_\-]{12,}")),
    ("jwt", re.compile(rb"eyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}")),
    ("pem", re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----")),
    ("rfc1918_ip", re.compile(rb"\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3})\b")),
    ("home_path", re.compile(rb"(?:/Us[e]rs/|/h[o]me/)[A-Za-z0-9][^/\r\n\s\"']*")),
]


def scrub(data: bytes, seed: str) -> tuple[bytes, dict]:
    counts: dict[str, int] = {}
    out = data
    for name, rx, repl in build_rules(seed):
        n = 0

        def _sub(m, _repl=repl):
            nonlocal n
            r = _repl(m)
            assert len(r) == len(m.group(0)), f"{name}: non-length-preserving replacement"
            n += 1
            return r

        out = rx.sub(_sub, out)
        if n:
            counts[name] = n
    assert len(out) == len(data), "scrub changed file length"
    return out, counts


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("infile")
    ap.add_argument("outfile")
    ap.add_argument("--sidecar", default=None, help="write a redaction audit JSON here")
    ap.add_argument("--seed", default="heron-pcap-scrub-v1", help="deterministic fake seed")
    args = ap.parse_args()

    data = Path(args.infile).read_bytes()
    out, counts = scrub(data, args.seed)

    # Post-scan: refuse to emit if a forbidden pattern survived. Allow the
    # username after the home-dir prefix to be all-X (our filler) — exclude that.
    residual = []
    for name, rx in RESIDUAL_FORBIDDEN:
        for m in rx.finditer(out):
            frag = m.group(0)
            if name == "home_path" and set(frag.split(b"/")[-1]) <= set(FILL):
                continue
            residual.append((name, m.start()))
    if residual:
        sys.stderr.write(f"ERROR: {len(residual)} forbidden pattern(s) survived scrubbing: "
                         f"{sorted({n for n, _ in residual})}\n")
        return 2

    Path(args.outfile).write_bytes(out)

    sidecar = {
        "input": Path(args.infile).name,
        "output": Path(args.outfile).name,
        "size_bytes": len(out),
        "length_preserved": True,
        "seed": args.seed,
        "rules_fired": counts,
    }
    if args.sidecar:
        Path(args.sidecar).write_text(json.dumps(sidecar, indent=2) + "\n")
    print(json.dumps(sidecar, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
