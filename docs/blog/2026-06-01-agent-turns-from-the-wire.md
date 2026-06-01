# Reconstructing agent turns from raw packets

*How Heron rebuilds what an AI agent is actually doing — tool calls, plan
loops, multi-leg proxy hops — from plaintext HTTP on the wire, with no SDK,
no proxy in the request path, and no cooperation from the workload.*

---

## The problem with watching agents

An agent run is not one LLM call. It's a loop: the planner calls the model,
the model asks for a tool, the tool result goes back, the planner calls
again — dozens or hundreds of times — until it submits an answer. When that
loop misbehaves in production (a tool call stalls, the planner oscillates
between two states, a downstream proxy silently swaps the model), the
evidence is spread across many HTTP calls that nobody stitched back together.

The usual ways to see this each cost you something:

- **SDK instrumentation** (Langfuse/LangSmith-style) needs every client to
  emit, and it sits in your code path.
- **A reverse proxy** (LiteLLM and friends) sees full bodies but is *in the
  request path* — if it falls over, so do your calls — and it only sees
  per-call data, not the turn.
- **OpenTelemetry from the server** needs the server to emit, and usually
  only gets partial bodies.

Heron takes a different bet: **read the bytes already on the wire.**

## Where Heron sits

Heron runs where the traffic is *already decrypted* — on the inference host,
behind the TLS terminator, or fed from a SPAN/TAP via
[cloud-probe](https://github.com/Netis/cloud-probe). It never sits in the
request path, so the observer can crash without breaking a single call, and
the workloads being observed need zero changes.

```
NIC / .pcap / cloud-probe (ZMQ)
        │
        ▼
   capture → flow dispatcher (hash by 5-tuple)
        │
        ▼
   N parallel workers: HTTP/SSE parse → wire-API decode → semantic extraction
        │
        ▼
   turn tracker  +  metrics aggregator  +  storage sink
        │
        ▼
       DuckDB ── REST API ── React console
```

## The hard parts

Turning packets into "this agent ran a 247-call plan, looped twice on the
edit tool, and one leg went through litellm to a vLLM backend" takes a few
non-obvious steps.

### 1. Reassemble HTTP/SSE from TCP, zero-copy

Packets arrive fragmented and interleaved across connections. A flow
dispatcher hashes by 5-tuple so every packet of one connection lands on the
same worker — parsing state stays local and lock-free. Each worker
reassembles the TCP stream and parses HTTP with a zero-copy parser, including
streaming `text/event-stream` (SSE) responses, which is where most of the
interesting token-timing signal lives.

### 2. Decode the wire API, not just "an HTTP call"

OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini
all look like HTTP POSTs but carry different shapes. Heron detects the
wire API and extracts a normalized `LlmCall`: model, tokens, finish reason,
TTFT, tool calls, and the raw body for drill-down. This is also where the
streaming-vs-non-streaming TTFT distinction matters — a non-streaming
response has no meaningful time-to-first-token, so Heron doesn't fabricate
one.

### 3. Stitch calls into a turn

This is the part nobody else does from the wire. Heron matches a per-agent
profile (Claude Code, Codex CLI, a generic tool-call-anchored fallback) to
group the call sequence into a single addressable **agent turn**, rolling up
tool surfaces, tool-call counts, and topology. A text-only one-shot call
stays on the calls page; a real tool-using loop becomes one turn you can open
and read end to end.

### 4. Fold multi-leg proxy hops

In a real fleet, one logical call shows up several times on the wire:
a client → a litellm proxy → a vLLM/SGLang backend, sometimes captured twice
more by `any`-interface double-capture on `br0` and `docker0`. Heron's
passive pair-sweeper folds these legs into one row by content + timing
fingerprint — deliberately *not* by topology, because docker bridges SNAT
the source IP and the proxy's listen-IP differs from its outbound-IP, so the
obvious "A.server_ip == B.client_ip" signal is unreliable. Content and timing
are what survive.

The result is a service graph that shows the call path you actually have —
clients → litellm → vLLM/SGLang — with each hop measured independently, and a
classifier that names what each endpoint serves (vLLM, SGLang, Ollama,
llama.cpp, LiteLLM) from the bytes, not from config someone told it.

## What you get

- **Agent turns**, not just calls — open a 247-call run and read the whole
  plan on a timeline.
- **TTFT · E2E latency · TPOT · token throughput · cache-hit ratio**, framed
  first at the agent layer, then per call.
- **Service topology** with proxy / inferred / client edges and
  auto-classified backends.
- **Full request/response bodies** captured for every call — the evidence is
  on the page, not behind a re-run.
- One **statically-linked binary** with the web console embedded; runs on any
  Linux (musl) or macOS, no libpcap/glibc install dance.

## Try it without a live interface

You don't need to point it at production to see what it does. Replay a pcap:

```bash
heron --pcap-file capture.pcap --no-retention
# open http://localhost:3000
```

Heron keeps the console up after the file drains so you can browse the
reconstructed turns. Apache-2.0, single binary, no telemetry:
**https://github.com/Netis/heron**
