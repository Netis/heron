# Launch posts — Heron

Ready-to-paste copy for the v0.4.0 launch. Tune the personal/“why I built
this” lines to your own voice before posting. **Do not** paste any internal
host/IP/credential anywhere — keep it to the public repo + public demo only.

Timing: post Show HN Tue–Thu ~8–10am US Pacific; cross-post to Reddit/X the
same morning and stay around to answer comments for the first 3–4 hours.

---

## Show HN

**Title:**
> Show HN: Heron – Reconstruct AI agent behavior from network packets (no SDK)

**Body:**
```
Heron is a passive observability tool for LLM/agent traffic. Instead of an
SDK or a proxy in the request path, it reads the plaintext HTTP already on
the wire (on the inference host, behind your TLS terminator, or from a
SPAN/TAP) and reconstructs what your agents are actually doing — tool calls,
multi-step plans, where time goes, where loops happen, who calls whom.

The part I couldn't find elsewhere: it stitches the call sequence into a
single addressable "agent turn" (Claude Code, Codex CLI, and a generic
tool-call profile), and folds multi-leg proxy hops (client → litellm →
vLLM/SGLang) into one row by content+timing fingerprint, then draws the
service graph and auto-classifies each backend from the bytes.

Why passive: the observer can crash without breaking the calls it watches,
and the workloads need zero changes. Trade-off is honest — you only see
TLS-terminated traffic, so you install it where the bytes are already
decrypted.

Decoders: OpenAI Chat/Responses, Anthropic Messages, Gemini — which covers
OpenAI/Azure/Anthropic/Bedrock/Vertex/Gemini and any OpenAI-compatible local
server (vLLM, SGLang, Ollama, llama.cpp).

Single statically-linked binary with the web console embedded; Linux (musl)
+ macOS. Apache-2.0, no telemetry.

You can try it with zero privileges by replaying a pcap:
  heron --pcap-file capture.pcap --no-retention
then open http://localhost:3000.

Repo: https://github.com/Netis/heron
How it works (turn reconstruction from packets): [link to the blog post]

Happy to answer questions about the TCP/SSE reassembly, the turn-stitching
profiles, or the proxy-hop folding.
```

---

## r/LocalLLaMA

**Title:**
> I built a passive monitor that reconstructs agent turns from your vLLM/SGLang traffic — no SDK, no proxy in the path

**Body:**
```
If you self-host inference (vLLM, SGLang, Ollama, llama.cpp) behind something
like litellm, you've probably had a hard time seeing what your agents are
actually doing across the call graph — and SDK/proxy approaches either need
code changes or sit in the request path.

Heron reads the plaintext HTTP on the wire (post-TLS, on the host or via a
tap) and rebuilds it: per-call detail with full bodies, agent *turns*
(the whole tool-call loop as one unit), TTFT/TPOT/throughput, and a service
graph that folds client → litellm → vLLM/SGLang hops into one row and
auto-classifies each backend from the bytes (it'll tell vLLM from SGLang from
Ollama without you configuring it).

Zero-privilege try: replay a pcap, open localhost:3000.

Single static binary + embedded console, Apache-2.0, no telemetry:
https://github.com/Netis/heron

Curious what backends/wire-APIs people here would want covered next.
```

---

## X / Twitter thread

```
1/ Heron: see what your AI agents actually do — reconstructed from network
packets. No SDK. No proxy in the request path. The observer can crash and
your calls keep working. 🪶 https://github.com/Netis/heron

2/ Most agent observability needs an SDK (code changes) or a reverse proxy
(in your request path, per-call only). Heron reads the plaintext HTTP already
on the wire — on the inference host or from a tap — and needs zero
cooperation from the workload.

3/ The hard part it solves: stitching the call sequence into one *agent turn*
(planner → tool → planner → tool…), so you open one row and read a 247-call
run end to end instead of joining HTTP logs in your warehouse.

4/ It also folds multi-leg proxy hops — client → litellm → vLLM/SGLang — into
a single row by content+timing (not topology; docker SNAT lies), and
auto-classifies each backend from the bytes.

5/ One static binary, embedded console, Linux+macOS, Apache-2.0, no
telemetry. Try it with zero privileges by replaying a pcap:
heron --pcap-file capture.pcap --no-retention → localhost:3000
```

---

## Where else to seed (low effort, high signal)

- Comment in the **LiteLLM / vLLM / SGLang** Discords/issues where people ask
  "how do I see what's flowing through my proxy" — link the service-graph
  feature specifically.
- PR into **awesome-llmops** and **awesome-observability** lists.
- r/devops, r/selfhosted, r/mlops with the angle that fits each (ops/topology
  for devops, single-binary self-host for selfhosted, spend/metrics for mlops).
- Lobsters (`ai`, `devops` tags) with the technical blog post, not the repo
  link, as the submission.
