# TokenScope

**LLM API observability from the network wire.** A passive, provider-side analyzer that turns LLM API traffic into structured performance, cost, and behavioral telemetry — without an SDK, sidecar, or proxy in the request path.

<!-- TODO: top-level console screenshot here -->

## What it does

TokenScope reads LLM API traffic (post-TLS, on the inference host or downstream of a TLS terminator), decodes the protocol — including SSE streaming and chunked encoding — reconstructs **agent turns** by stitching multi-call interactions together, and emits queryable metrics and per-call detail through a built-in web console.

```
NIC / .pcap file / cloud-probe (ZMQ)
        │
        ▼
   capture → flow dispatcher (hash by 5-tuple)
        │
        ▼
   N parallel workers: HTTP/SSE parse → wire-API detection → semantic extraction
        │
        ▼
   turn tracker  +  metrics aggregator  +  storage sink
        │
        ▼
       DuckDB ─── REST API ─── React console (localhost:3000)
```

Same connection's packets always land on the same worker, so parsing state is local and lock-free. Multiple independent pipelines can run side-by-side — e.g., low-latency local capture isolated from bursty cloud-probe ingress.

## Why not an SDK / proxy / OpenTelemetry?

| Approach                   | In request path | Needs client cooperation | Sees full bodies |
| -------------------------- | :-------------: | :----------------------: | :--------------: |
| SDK instrumentation        |       yes       |    every client must     |       yes        |
| Reverse proxy (LiteLLM …)  |       yes       |   clients point at it    |       yes        |
| OpenTelemetry from server  |       yes       |     server must emit     |     partial      |
| **TokenScope**             |     **no**      |          **no**          |   **yes**¹       |

¹ TLS-terminated traffic only — TokenScope sees plaintext HTTP. Install it where the traffic is already decrypted: on the inference host, behind the TLS terminator, or fed by [cloud-probe](https://github.com/Netis/cloud-probe) from a SPAN/TAP point.

The trade-off is honest: you give up cross-cluster client tracing, you get a single passive evidence chain that can't break the call when the observer fails, and that requires zero cooperation from the workloads being observed.

## What ships in v0.1

**Ingress**
- libpcap on a live interface (BPF-filtered)
- Replay from `.pcap` files (any speed)
- ZMQ from [cloud-probe](https://github.com/Netis/cloud-probe) for hosts you can't install on directly

**Wire-API decoders**
- OpenAI Chat Completions (`/v1/chat/completions`)
- OpenAI Responses (`/v1/responses`)
- Anthropic Messages (`/v1/messages`)

This covers OpenAI direct, Azure OpenAI, Anthropic direct, AWS Bedrock / GCP Vertex (Anthropic wire), and any OpenAI-compatible local server — vLLM, Ollama, llama.cpp's server, LM Studio, etc. Gemini's native API is not yet decoded.

**Agent-turn reconstruction** with named profiles for **Claude CLI** (Claude Code) and **OpenAI Codex CLI**, a generic profile for everything else, plus an experimental OpenClaw profile. Turns stitch multi-call agent interactions (tool calls, follow-ups) into a single addressable unit.

**Metrics** (sliding-window, per LLM Call): TTFT · E2E latency · TPOT · token throughput · call rate · active calls · call error rate · prompt-cache hit ratio. See [glossary](docs/glossary.md) for what each means and why.

**Storage** in DuckDB (default, embedded, single-file) with per-table retention enabled out of the box. Pluggable backend trait — PostgreSQL and ClickHouse are designed but not yet wired.

**Console** at `http://localhost:3000`: overview · performance · traffic · models · errors · LLM calls (with full request/response body drill-down) · raw HTTP exchanges · agent turns · agent sessions · pipeline-health debug views.

**Distribution**: prebuilt static binaries for Linux musl (x86_64 + aarch64) and macOS (Intel + Apple Silicon). Web console is **embedded in the binary** — single artifact, no separate frontend deploy.

## Who it's for

- **LLM provider ops & on-prem inference operators** — measure your fleet from ground truth, not from what each SDK reports
- **Agent developers** — debug stalled tool calls and detect agent loops without modifying the agent
- **FinOps & engineering managers** — attribute spend across teams/repos/projects from real traffic, not periodic exports
- **Compliance & security** — capture-once evidence chain of what crossed the wire

## Quickstart

```bash
# Install (Linux/macOS, no sudo, user-local)
curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh \
  | INSTALL_DIR="$HOME/.local" sh

# Linux: grant capture privileges to the binary (no sudo at runtime)
sudo setcap cap_net_raw,cap_net_admin=eip ~/.local/bin/tokenscope

# Capture from a live interface
tokenscope -i eth0 --bpf-filter "tcp port 8000"

# ...or replay a pcap (no privileges needed)
tokenscope --pcap-file capture.pcap
```

Then open <http://localhost:3000>.

> TokenScope sees **plaintext** HTTP. The BPF filter targets the *internal* port your inference server listens on (vLLM 8000, Ollama 11434, your TLS-terminator's backend pool, …) — never `:443`.

For systemd deployment, capability options, macOS BPF setup, and uninstall, see [docs/install.md](docs/install.md).

## Documentation

- [Install](docs/install.md) — one-line installer, systemd, capabilities
- [Configure](docs/configure.md) — pipelines, sources, storage, retention, BPF filters
- [Glossary](docs/glossary.md) — what every metric means
- [Architecture](docs/design/01-architecture.md) — pipeline design and trade-offs
- [Mission](docs/mission.md) — long-arc vision and BPC heritage

## Roadmap

The v0.1 surface is the foundation layer (Ops use cases). On the way:

- **Storage** — PostgreSQL and ClickHouse backends (schemas already designed)
- **Wire APIs** — Gemini native API, more provider-specific extensions
- **Export** — OpenTelemetry, Prometheus remote-write, raw event firehose
- **Higher layers** — PII / data-boundary signals, business-outcome correlation, vendor SLA reports

See [docs/mission.md](docs/mission.md) for the full ladder.

## Project origin

TokenScope is an open-source project from **Netis Systems**, a Shenzhen-based NPM/BPC vendor (founded 2000) with two decades of packet-evidence observability work for regulated enterprise. The project's ambition is **vendor-neutral**: useful to anyone operating LLM API traffic, not Netis customers alone. [cloud-probe](https://github.com/Netis/cloud-probe) is one supported ingress, not a requirement.

## Contributing

Bug reports and PRs welcome. Before opening a PR, run:

```bash
just build all       # single binary with embedded console
just quality all     # rust fmt + clippy + ts lint + tsc
just test all        # cargo test (all crates)
```

Run `just help` for the full menu. Design docs under [docs/design/](docs/design/) describe the per-module contract — read the relevant one before changing anything load-bearing.

## License

[Apache 2.0](LICENSE).
