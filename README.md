# TokenScope

**LLM observability from the wire.** A passive, provider-side protocol analyzer that turns LLM API traffic into actionable behavioral and business intelligence — no agents in the request path, no SDK in the way.

## What it is

TokenScope reads LLM API traffic — OpenAI, Anthropic, Azure OpenAI, Gemini, vLLM, Ollama, and other OpenAI-compatible endpoints — from a packet capture or a ZMQ-forwarded probe, decodes the protocol (including streaming SSE), reconstructs agent turns, and emits structured telemetry. Metrics (TTFT, Token Throughput, Call Rate, Active Calls, Call Error Rate, token usage, cache hits) land in DuckDB / PostgreSQL / ClickHouse; live sessions stream through a web console.

Think of it as the Wireshark layer for LLM-era traffic: ground truth, captured once, consumable by many tools.

## Who it's for

- **Ops.** Platform SRE and LLM provider infra — keep inference clusters healthy, tune cache and batch shape, plan capacity from real traffic.
- **Devs.** Individual developers and agent builders — understand why an agent stalled, compare frameworks, debug tool calls without touching the agent.
- **Dev-teams.** Engineering managers and FinOps — attribute AI spend across projects, repos, teams, and models; spot portfolio inefficiencies.
- **BU, Compliance, Procurement.** Business outcome attribution, data-governance evidence, vendor SLA measurement. (Built on the same capture; later on the roadmap.)

## The BPC leap

TokenScope extends the Behavioral Packet Capture idea — inferring business behavior from network evidence — into the AI era. Classical BPC struggled with opaque application payloads; LLM API traffic is already structured intent, plan, and outcome. AI-assisted analysis over the wire makes business-layer observability tractable, without an SDK in the request path.

## Project origin

TokenScope is an open-source project from Netis Systems, continuing a two-decade lineage of packet-evidence NPM/BPC/AIOps for regulated enterprise. The project's ambition is **vendor-neutral**: useful to anyone operating LLM API traffic, not Netis customers alone. Cloud-probe (`github.com/Netis/cloud-probe`) is one supported ingress, not a requirement.

## Quickstart

See [`docs/`](docs/) for deployment, configuration, and usage. Architecture overview in [`docs/design/01-architecture.md`](docs/design/01-architecture.md). Terminology reference in [`docs/glossary.md`](docs/glossary.md). Longer mission statement in [`docs/mission.md`](docs/mission.md).

## License

See [LICENSE](LICENSE).
