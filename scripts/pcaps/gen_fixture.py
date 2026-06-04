#!/usr/bin/env python3
"""Deterministically synthesize Heron pcap corpus fixtures.

We control the request headers/body AND the response, so this generates the
whole `anthropic` column (claude-cli / openclaw / hermes / generic) and the
**cliproxy mixed-format** cell (OpenAI-Chat request → Anthropic-shaped response,
issue #96) without any live server, real API key, or capture — fully
reproducible and secret-free by construction.

Output is a classic little-endian pcap (linktype 1 = Ethernet) that Heron's
PcapFileSource reads directly. Frames are Ethernet/IPv4/TCP over loopback
(127.0.0.1) with a real 3-way handshake, monotonic per-direction sequence
numbers, MTU-sized data segments (to exercise reassembly), and a FIN teardown.
Checksums are zero — Heron reassembles by 5-tuple+seq and does not validate
them.

Usage:
  gen_fixture.py --scenario <name> --out <file.pcap>
  gen_fixture.py --list
  gen_fixture.py --all --corpus-dir testdata/pcaps/corpus   # regenerate all
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# pcap + TCP/IP framing  (classic pcap, linktype 1 Ethernet, loopback)
# ---------------------------------------------------------------------------

CLIENT_IP = "127.0.0.1"
SERVER_IP = "127.0.0.1"
CLIENT_PORT = 50000
SERVER_PORT = 8317
MSS = 1400  # segment size for data payloads


def _ip4(addr: str) -> bytes:
    return bytes(int(o) for o in addr.split("."))


def _eth(client_to_server: bool) -> bytes:
    # dst mac, src mac, ethertype IPv4. Distinct per direction for realism.
    a = bytes([0x02, 0, 0, 0, 0, 0x01])
    b = bytes([0x02, 0, 0, 0, 0, 0x02])
    return (b + a if client_to_server else a + b) + b"\x08\x00"


def _ipv4(payload_len: int, c2s: bool, ip_id: int) -> bytes:
    total = 20 + payload_len
    src, dst = (CLIENT_IP, SERVER_IP) if c2s else (SERVER_IP, CLIENT_IP)
    return struct.pack(
        ">BBHHHBBH4s4s",
        0x45, 0x00, total, ip_id & 0xFFFF, 0x4000, 64, 6, 0, _ip4(src), _ip4(dst)
    )


def _tcp(c2s: bool, seq: int, ack: int, flags: int, payload_len: int) -> bytes:
    sport, dport = (CLIENT_PORT, SERVER_PORT) if c2s else (SERVER_PORT, CLIENT_PORT)
    off_flags = (5 << 12) | flags  # data offset 5 words (20 bytes), no options
    return struct.pack(
        ">HHIIHHHH", sport, dport, seq & 0xFFFFFFFF, ack & 0xFFFFFFFF,
        off_flags, 65535, 0, 0
    )


FIN, SYN, PSH, ACK = 0x01, 0x02, 0x08, 0x10


class PcapWriter:
    def __init__(self):
        self.records: list[bytes] = []
        self.ip_id = 1
        self.ts = 1_700_000_000  # fixed base; +1us per packet (deterministic)
        self.tick = 0

    def _emit(self, c2s: bool, seq: int, ack: int, flags: int, payload: bytes):
        frame = _eth(c2s) + _ipv4(20 + len(payload), c2s, self.ip_id) + \
            _tcp(c2s, seq, ack, flags, len(payload)) + payload
        self.ip_id += 1
        self.tick += 1
        rec = struct.pack("<IIII", self.ts, self.tick, len(frame), len(frame)) + frame
        self.records.append(rec)

    def connection(self, client_stream: bytes, server_stream: bytes):
        """One TCP connection: handshake, client bytes, server bytes, teardown.
        Per-direction byte order is preserved (that's all Heron's reassembler
        needs); we interleave a single request burst then a single response
        burst, which is sufficient for the joiner to pair exchanges in order."""
        c_seq, s_seq = 1000, 5000
        # handshake
        self._emit(True, c_seq, 0, SYN, b"")
        c_seq += 1
        self._emit(False, s_seq, c_seq, SYN | ACK, b"")
        s_seq += 1
        self._emit(True, c_seq, s_seq, ACK, b"")
        # client → server data
        for i in range(0, len(client_stream), MSS):
            seg = client_stream[i:i + MSS]
            self._emit(True, c_seq, s_seq, PSH | ACK, seg)
            c_seq += len(seg)
        # server → client data
        for i in range(0, len(server_stream), MSS):
            seg = server_stream[i:i + MSS]
            self._emit(False, s_seq, c_seq, PSH | ACK, seg)
            s_seq += len(seg)
        # teardown
        self._emit(True, c_seq, s_seq, FIN | ACK, b"")
        c_seq += 1
        self._emit(False, s_seq, c_seq, FIN | ACK, b"")

    def to_bytes(self) -> bytes:
        gh = struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1)
        return gh + b"".join(self.records)


# ---------------------------------------------------------------------------
# HTTP framing helpers
# ---------------------------------------------------------------------------

def http_request(method: str, path: str, headers: dict, body: bytes) -> bytes:
    lines = [f"{method} {path} HTTP/1.1"]
    h = dict(headers)
    h.setdefault("Host", "127.0.0.1:8317")
    h["Content-Length"] = str(len(body))
    for k, v in h.items():
        lines.append(f"{k}: {v}")
    return ("\r\n".join(lines) + "\r\n\r\n").encode() + body


def http_response_json(body: bytes, content_type="application/json") -> bytes:
    head = (
        "HTTP/1.1 200 OK\r\n"
        f"content-type: {content_type}\r\n"
        f"content-length: {len(body)}\r\n\r\n"
    )
    return head.encode() + body


def http_response_sse(events: list[bytes]) -> bytes:
    """SSE over chunked transfer-encoding (each event = one chunk)."""
    head = (
        "HTTP/1.1 200 OK\r\n"
        "content-type: text/event-stream; charset=utf-8\r\n"
        "Transfer-Encoding: chunked\r\n\r\n"
    ).encode()
    out = bytearray(head)
    for ev in events:
        out += f"{len(ev):x}\r\n".encode() + ev + b"\r\n"
    out += b"0\r\n\r\n"
    return bytes(out)


def sse(event: str, data: dict) -> bytes:
    return f"event: {event}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n".encode()


# ---------------------------------------------------------------------------
# Faithful Anthropic Messages responder
# ---------------------------------------------------------------------------

def anthropic_nonstream(text="hello from the assistant",
                        tool=None, stop_reason="end_turn",
                        usage=None, model="claude-sonnet-4-20250514") -> bytes:
    content = [{"type": "text", "text": text}]
    if tool:
        content.append({"type": "tool_use", "id": tool["id"], "name": tool["name"],
                        "input": tool["input"]})
    u = usage or {"input_tokens": 1200, "output_tokens": 85,
                  "cache_read_input_tokens": 1024, "cache_creation_input_tokens": 0}
    return json.dumps({
        "id": "msg_0123456789abcdef", "type": "message", "role": "assistant",
        "model": model, "content": content,
        "stop_reason": stop_reason, "stop_sequence": None, "usage": u,
    }, separators=(",", ":")).encode()


def anthropic_stream(text="Let me check that.", tools=None,
                     stop_reason="end_turn", usage=None,
                     model="claude-sonnet-4-20250514") -> list[bytes]:
    """Full Anthropic streaming event sequence. `tools` is a list of
    {id,name,input} for (possibly parallel) tool_use blocks — every
    content_block_start arrives, then input_json_delta per index (exercises
    the index-keyed SSE accumulator)."""
    tools = tools or []
    u = usage or {"input_tokens": 1200, "output_tokens": 1,
                  "cache_read_input_tokens": 1024, "cache_creation_input_tokens": 0}
    out = [sse("message_start", {"type": "message_start", "message": {
        "id": "msg_0123456789abcdef", "type": "message", "role": "assistant",
        "model": model, "content": [], "stop_reason": None,
        "stop_sequence": None, "usage": u}})]
    # text block index 0
    out.append(sse("content_block_start", {"type": "content_block_start",
                "index": 0, "content_block": {"type": "text", "text": ""}}))
    for piece in text.split(" "):
        out.append(sse("content_block_delta", {"type": "content_block_delta",
                    "index": 0, "delta": {"type": "text_delta", "text": piece + " "}}))
    out.append(sse("content_block_stop", {"type": "content_block_stop", "index": 0}))
    # tool_use blocks: all starts first, then interleaved input_json_delta
    for n, t in enumerate(tools, start=1):
        out.append(sse("content_block_start", {"type": "content_block_start",
                    "index": n, "content_block": {"type": "tool_use",
                    "id": t["id"], "name": t["name"], "input": {}}}))
    for n, t in enumerate(tools, start=1):
        js = json.dumps(t["input"], separators=(",", ":"))
        mid = len(js) // 2
        for part in (js[:mid], js[mid:]):
            out.append(sse("content_block_delta", {"type": "content_block_delta",
                        "index": n, "delta": {"type": "input_json_delta",
                        "partial_json": part}}))
    for n in range(1, len(tools) + 1):
        out.append(sse("content_block_stop", {"type": "content_block_stop", "index": n}))
    out.append(sse("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                "usage": {"output_tokens": (usage or {}).get("output_tokens", 85)}}))
    out.append(sse("message_stop", {"type": "message_stop"}))
    return out


# ---------------------------------------------------------------------------
# scenarios → list of (request_bytes, response_bytes) on one connection
# ---------------------------------------------------------------------------

# Auth values are deliberately NON-secret-shaped (no sk-/sk-ant- prefix) so the
# synthesized fixtures are secret-free by construction and pass the corpus
# leakage gate without a scrub pass. Wire-API + agent detection here keys off
# route + anthropic-version + User-Agent/session headers, not the token prefix.
FAKE_ANT_KEY = "REDACTED-anthropic-test-token"
FAKE_OAI_KEY = "REDACTED-openai-test-token"
SESSION = "11111111-2222-3333-4444-555555555555"

# A claude-cli MAIN agent request carries the "Agent" tool (the sub-agent
# spawner); sub-agent requests lack it (h-llm claude_cli::looks_like_subagent).
# Include it so this synthesizes as a single-agent main turn, not an orphan.
CLAUDE_TOOLS = [
    {"name": "Agent", "description": "spawn a sub-agent",
     "input_schema": {"type": "object", "properties": {"prompt": {"type": "string"}}}},
    {"name": "Read", "description": "read a file",
     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
]


def scen_claude_cli_anthropic_stream():
    """claude-cli over Anthropic /v1/messages, streaming, one tool roundtrip
    → 1 complete turn. Exercises claude-cli header detection + index-keyed SSE
    accumulator + usage/cache extraction."""
    hdr = {
        "User-Agent": "claude-cli/2.1.0 (external, cli)",
        "X-Claude-Code-Session-Id": SESSION,
        "anthropic-version": "2023-06-01",
        "authorization": f"Bearer {FAKE_ANT_KEY}",
        "content-type": "application/json",
    }
    # exchange 1: user prompt → assistant tool_use (stop_reason tool_use)
    req1 = http_request("POST", "/v1/messages", hdr, json.dumps({
        "model": "claude-sonnet-4-20250514", "max_tokens": 1024,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "read config.toml"}]}],
        "tools": CLAUDE_TOOLS, "stream": True}, separators=(",", ":")).encode())
    resp1 = http_response_sse(anthropic_stream(
        text="I'll read it.",
        tools=[{"id": "toolu_01aaaa", "name": "Read", "input": {"path": "config.toml"}}],
        stop_reason="tool_use",
        usage={"input_tokens": 1500, "output_tokens": 40,
               "cache_read_input_tokens": 1024, "cache_creation_input_tokens": 256}))
    # exchange 2: tool_result → assistant final text (stop_reason end_turn)
    req2 = http_request("POST", "/v1/messages", hdr, json.dumps({
        "model": "claude-sonnet-4-20250514", "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "read config.toml"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_01aaaa",
                "name": "Read", "input": {"path": "config.toml"}}]},
            {"role": "user", "content": [{"type": "tool_result",
                "tool_use_id": "toolu_01aaaa", "content": "port=8317"}]}],
        "tools": CLAUDE_TOOLS, "stream": True}, separators=(",", ":")).encode())
    resp2 = http_response_sse(anthropic_stream(
        text="The port is 8317.", tools=[], stop_reason="end_turn",
        usage={"input_tokens": 1600, "output_tokens": 30,
               "cache_read_input_tokens": 1500, "cache_creation_input_tokens": 0}))
    return [(req1, resp1), (req2, resp2)]


# The exact #96 shape: the proxy speaks the OpenAI Chat API (response is
# chat.completion-SHAPED — choices[].message, finish_reason) but the usage block
# carries ANTHROPIC field names (input_tokens / output_tokens /
# cache_read_input_tokens) because it forwarded the upstream Anthropic usage
# without remapping. Heron detects openai-chat from the request; the chat
# extractor must fall back to the Anthropic usage names.
CLIPROXY_USAGE = {"input_tokens": 309, "output_tokens": 37, "cache_read_input_tokens": 256}


def _oai_chat_nonstream_body(text, finish_reason, usage):
    return json.dumps({
        "id": "chatcmpl-proxy", "object": "chat.completion", "model": OAI_MODEL,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": finish_reason}],
        "usage": usage,
    }, separators=(",", ":")).encode()


def scen_cliproxy_mixed_format():
    """cliproxy mixed-format (#96), NON-streaming: OpenAI-Chat request + an
    OpenAI chat.completion-SHAPED response whose `usage` uses ANTHROPIC field
    names. Detected openai-chat; usage falls back to input_tokens / output_tokens
    / cache_read_input_tokens."""
    hdr = {"User-Agent": "openai-python/1.40.0",
           "authorization": f"Bearer {FAKE_OAI_KEY}", "content-type": "application/json"}
    req = http_request("POST", "/v1/chat/completions", hdr, json.dumps({
        "model": OAI_MODEL, "stream": False,
        "messages": [{"role": "user", "content": "hello"}]}, separators=(",", ":")).encode())
    resp = http_response_json(_oai_chat_nonstream_body("hi there", "stop", CLIPROXY_USAGE))
    return [(req, resp)]


def scen_cliproxy_mixed_stream():
    """cliproxy mixed-format (#96), STREAMING: OpenAI-Chat request + OpenAI SSE
    chunks whose final usage chunk uses ANTHROPIC field names. Exercises the
    SSE-path usage fallback (a separate code path from the non-stream one)."""
    hdr = {"User-Agent": "openai-python/1.40.0",
           "authorization": f"Bearer {FAKE_OAI_KEY}", "content-type": "application/json"}
    req = http_request("POST", "/v1/chat/completions", hdr, json.dumps({
        "model": OAI_MODEL, "stream": True, "stream_options": {"include_usage": True},
        "messages": [{"role": "user", "content": "hello"}]}, separators=(",", ":")).encode())
    resp = http_response_sse(oai_chat_stream("hi there", [], "stop", CLIPROXY_USAGE))
    return [(req, resp)]


# --- shared anthropic roundtrip builder (fills the rest of the column) -------

def _tool_def(name):
    return {"name": name, "description": f"{name} tool",
            "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}}}


def ant_req(tool_names, messages, stream, headers):
    h = {"anthropic-version": "2023-06-01",
         "authorization": f"Bearer {FAKE_ANT_KEY}", "content-type": "application/json"}
    h.update(headers)
    body = {"model": "claude-sonnet-4-20250514", "max_tokens": 1024,
            "messages": messages, "tools": [_tool_def(n) for n in tool_names],
            "stream": stream}
    return http_request("POST", "/v1/messages", h, json.dumps(body, separators=(",", ":")).encode())


def ant_roundtrip(tool_names, headers, stream, tools_called, user_text):
    """2-exchange tool roundtrip on one connection → 1 complete turn.
    tools_called = assistant tool_use blocks emitted in exchange 1 (>1 = parallel)."""
    u_msg = {"role": "user", "content": [{"type": "text", "text": user_text}]}
    req1 = ant_req(tool_names, [u_msg], stream, headers)
    u1 = {"input_tokens": 1400, "output_tokens": 35,
          "cache_read_input_tokens": 512, "cache_creation_input_tokens": 0}
    if stream:
        resp1 = http_response_sse(anthropic_stream(
            text="working on it", tools=tools_called, stop_reason="tool_use", usage=u1))
    else:
        resp1 = http_response_json(anthropic_nonstream(
            text="working on it", tool=tools_called[0], stop_reason="tool_use", usage=u1))
    assistant_blocks = [{"type": "tool_use", "id": t["id"], "name": t["name"],
                         "input": t["input"]} for t in tools_called]
    tool_results = [{"type": "tool_result", "tool_use_id": t["id"], "content": "ok"}
                    for t in tools_called]
    msgs2 = [u_msg, {"role": "assistant", "content": assistant_blocks},
             {"role": "user", "content": tool_results}]
    req2 = ant_req(tool_names, msgs2, stream, headers)
    u2 = {"input_tokens": 1500, "output_tokens": 25,
          "cache_read_input_tokens": 1400, "cache_creation_input_tokens": 0}
    if stream:
        resp2 = http_response_sse(anthropic_stream(
            text="all done", tools=[], stop_reason="end_turn", usage=u2))
    else:
        resp2 = http_response_json(anthropic_nonstream(
            text="all done", tool=None, stop_reason="end_turn", usage=u2))
    return [(req1, resp1), (req2, resp2)]


def scen_openclaw_anthropic_parallel():
    """openclaw over Anthropic, streaming, PARALLEL tool_use (two blocks) →
    1 complete openclaw turn. Body-fingerprint match (sessions_spawn+subagents);
    parallel tool_use exercises the index-keyed SSE accumulator."""
    return ant_roundtrip(
        tool_names=["sessions_spawn", "subagents", "Read", "Grep"],
        headers={"User-Agent": "node"}, stream=True,
        tools_called=[{"id": "toolu_par1", "name": "Read", "input": {"path": "a.txt"}},
                      {"id": "toolu_par2", "name": "Grep", "input": {"pattern": "foo"}}],
        user_text="search the repo")


def scen_hermes_anthropic():
    """hermes over Anthropic, streaming → 1 complete hermes turn. Body
    fingerprint (skill_view + delegate_task); no Hermes-specific UA."""
    return ant_roundtrip(
        tool_names=["skill_view", "delegate_task", "Read"],
        headers={"User-Agent": "anthropic-sdk-python/0.40.0"}, stream=True,
        tools_called=[{"id": "toolu_herm1", "name": "Read", "input": {"path": "x"}}],
        user_text="use a skill")


def scen_generic_anthropic():
    """generic fallback over Anthropic (no claude-cli headers, no openclaw/hermes
    markers, no Agent tool) → 1 complete generic turn; session synthesized from
    the first tool_use id."""
    return ant_roundtrip(
        tool_names=["Read"],
        headers={"User-Agent": "some-sdk/1.0"}, stream=True,
        tools_called=[{"id": "toolu_gen1", "name": "Read", "input": {"path": "x"}}],
        user_text="read a file")


def scen_claude_cli_anthropic_nonstream():
    """claude-cli over Anthropic, NON-streaming (Content-Length JSON) → 1
    complete claude-cli turn. Exercises the non-stream Anthropic extractor."""
    return ant_roundtrip(
        tool_names=["Agent", "Read"],
        headers={"User-Agent": "claude-cli/2.1.0 (external, cli)",
                 "X-Claude-Code-Session-Id": SESSION}, stream=False,
        tools_called=[{"id": "toolu_ns1", "name": "Read", "input": {"path": "x"}}],
        user_text="read it (nonstream)")


# --- openai column: chat (streaming) + responses (non-stream) ---------------

OAI_MODEL = "gpt-4o-2024-08-06"


def _oai_tool(name):
    return {"type": "function", "function": {"name": name, "description": f"{name} tool",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}}


def oai_chat_request(tool_names, messages, headers, stream=True):
    h = {"authorization": f"Bearer {FAKE_OAI_KEY}", "content-type": "application/json"}
    h.update(headers)
    body = {"model": OAI_MODEL, "messages": messages,
            "tools": [_oai_tool(n) for n in tool_names], "stream": stream,
            "stream_options": {"include_usage": True}}
    return http_request("POST", "/v1/chat/completions", h,
                        json.dumps(body, separators=(",", ":")).encode())


def _chunk(choices, usage=None):
    d = {"id": "chatcmpl-synth", "object": "chat.completion.chunk",
         "model": OAI_MODEL, "choices": choices}
    if usage is not None:
        d["usage"] = usage
    return f"data: {json.dumps(d, separators=(',', ':'))}\n\n".encode()


def oai_chat_stream(text, tool_calls, finish_reason, usage):
    out = [_chunk([{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}])]
    for w in (text.split(" ") if text else []):
        out.append(_chunk([{"index": 0, "delta": {"content": w + " "}, "finish_reason": None}]))
    for k, tc in enumerate(tool_calls):
        js = json.dumps(tc["arguments"], separators=(",", ":"))
        mid = len(js) // 2
        out.append(_chunk([{"index": 0, "delta": {"tool_calls": [{"index": k, "id": tc["id"],
                    "type": "function", "function": {"name": tc["name"], "arguments": ""}}]},
                    "finish_reason": None}]))
        out.append(_chunk([{"index": 0, "delta": {"tool_calls": [{"index": k,
                    "function": {"arguments": js[:mid]}}]}, "finish_reason": None}]))
        out.append(_chunk([{"index": 0, "delta": {"tool_calls": [{"index": k,
                    "function": {"arguments": js[mid:]}}]}, "finish_reason": None}]))
    out.append(_chunk([{"index": 0, "delta": {}, "finish_reason": finish_reason}]))
    out.append(_chunk([], usage=usage))
    out.append(b"data: [DONE]\n\n")
    return out


OAI_USAGE1 = {"prompt_tokens": 1400, "completion_tokens": 35, "total_tokens": 1435,
              "prompt_tokens_details": {"cached_tokens": 512}}
OAI_USAGE2 = {"prompt_tokens": 1500, "completion_tokens": 25, "total_tokens": 1525,
              "prompt_tokens_details": {"cached_tokens": 1400}}

CHAT_AGENTS = {
    "opencode": {"headers": {"User-Agent": "opencode/1.14.50 ai-sdk/provider-utils/4.0.23",
                             "x-session-affinity": "ses_synth0001"}, "tools": ["Read"]},
    "openclaw": {"headers": {"User-Agent": "node"},
                 "tools": ["sessions_spawn", "subagents", "Read"]},
    "hermes":   {"headers": {"User-Agent": "openai-python/1.40.0"},
                 "tools": ["skill_view", "delegate_task", "Read"]},
    "generic":  {"headers": {"User-Agent": "some-sdk/1.0"}, "tools": ["Read"]},
}


def scen_openai_chat(agent):
    """openai-chat streaming tool roundtrip → 1 complete turn for the given
    agent (detection via headers for opencode, body fingerprint for
    openclaw/hermes, fallback for generic)."""
    cfg = CHAT_AGENTS[agent]
    u_msg = {"role": "user", "content": "do the thing"}
    tc = {"id": "call_synth01", "name": "Read", "arguments": {"path": "x"}}
    req1 = oai_chat_request(cfg["tools"], [u_msg], cfg["headers"])
    resp1 = http_response_sse(oai_chat_stream("on it", [tc], "tool_calls", OAI_USAGE1))
    assistant = {"role": "assistant", "content": None, "tool_calls": [
        {"id": tc["id"], "type": "function", "function": {"name": tc["name"],
         "arguments": json.dumps(tc["arguments"], separators=(",", ":"))}}]}
    toolmsg = {"role": "tool", "tool_call_id": tc["id"], "content": "ok"}
    req2 = oai_chat_request(cfg["tools"], [u_msg, assistant, toolmsg], cfg["headers"])
    resp2 = http_response_sse(oai_chat_stream("all done", [], "stop", OAI_USAGE2))
    return [(req1, resp1), (req2, resp2)]


def oai_responses_request(tool_names, input_items, headers):
    h = {"authorization": f"Bearer {FAKE_OAI_KEY}", "content-type": "application/json"}
    h.update(headers)
    body = {"model": OAI_MODEL, "input": input_items, "stream": False,
            "tools": [{"type": "function", "name": n, "description": f"{n} tool",
                       "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}
                      for n in tool_names]}
    return http_request("POST", "/v1/responses", h,
                        json.dumps(body, separators=(",", ":")).encode())


def responses_json(output, usage):
    return json.dumps({"id": "resp_synth", "object": "response", "status": "completed",
                       "model": OAI_MODEL, "output": output, "usage": usage},
                      separators=(",", ":")).encode()


RESP_USAGE1 = {"input_tokens": 1400, "output_tokens": 35, "input_tokens_details": {"cached_tokens": 512}}
RESP_USAGE2 = {"input_tokens": 1500, "output_tokens": 25, "input_tokens_details": {"cached_tokens": 1400}}


def scen_openai_responses(agent):
    """openai-responses non-stream tool roundtrip → 1 complete turn. codex via
    Originator + X-Codex-Turn-Metadata; generic via fallback. ex1 output carries
    a function_call (not terminal); ex2 output is a message (terminal)."""
    if agent == "codex":
        headers = {"User-Agent": "codex_cli_rs/0.41.0", "Originator": "codex_cli_rs",
                   "X-Codex-Turn-Metadata": json.dumps({"session_id": SESSION})}
        tool_names = ["shell"]
    else:
        headers = {"User-Agent": "openai-python/1.40.0"}
        tool_names = ["lookup"]
    user_item = {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "do the thing"}]}
    req1 = oai_responses_request(tool_names, [user_item], headers)
    fc = {"type": "function_call", "id": "fc_synth01", "call_id": "call_synth01",
          "name": tool_names[0], "arguments": json.dumps({"x": "y"}, separators=(",", ":"))}
    resp1 = http_response_json(responses_json([fc], RESP_USAGE1))
    fc_out = {"type": "function_call_output", "call_id": "call_synth01", "output": "ok"}
    msg = {"type": "message", "role": "assistant",
           "content": [{"type": "output_text", "text": "all done"}]}
    req2 = oai_responses_request(tool_names, [user_item, fc, fc_out], headers)
    resp2 = http_response_json(responses_json([msg], RESP_USAGE2))
    return [(req1, resp1), (req2, resp2)]


SCENARIOS = {
    "claude-cli-anthropic-stream": scen_claude_cli_anthropic_stream,
    "claude-cli-anthropic-nonstream": scen_claude_cli_anthropic_nonstream,
    "openclaw-anthropic-parallel": scen_openclaw_anthropic_parallel,
    "hermes-anthropic": scen_hermes_anthropic,
    "generic-anthropic": scen_generic_anthropic,
    "cliproxy-mixed-format": scen_cliproxy_mixed_format,
    "cliproxy-mixed-format-stream": scen_cliproxy_mixed_stream,
    # openai column (below)
    "opencode-openai-chat": lambda: scen_openai_chat("opencode"),
    "openclaw-openai-chat": lambda: scen_openai_chat("openclaw"),
    "hermes-openai-chat": lambda: scen_openai_chat("hermes"),
    "generic-openai-chat": lambda: scen_openai_chat("generic"),
    "codex-responses": lambda: scen_openai_responses("codex"),
    "generic-openai-responses": lambda: scen_openai_responses("generic"),
}


def build(scenario: str) -> bytes:
    exchanges = SCENARIOS[scenario]()
    w = PcapWriter()
    client = b"".join(req for req, _ in exchanges)
    server = b"".join(resp for _, resp in exchanges)
    w.connection(client, server)
    return w.to_bytes()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenario")
    ap.add_argument("--out")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--corpus-dir", default="testdata/pcaps/corpus")
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()

    if args.list:
        for k in SCENARIOS:
            print(k)
        return 0
    if args.all:
        d = Path(args.corpus_dir)
        d.mkdir(parents=True, exist_ok=True)
        for name in SCENARIOS:
            p = d / f"{name}.pcap"
            p.write_bytes(build(name))
            print(f"wrote {p} ({p.stat().st_size} bytes)")
        return 0
    if not args.scenario or not args.out:
        ap.error("need --scenario and --out (or --all / --list)")
    Path(args.out).write_bytes(build(args.scenario))
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
