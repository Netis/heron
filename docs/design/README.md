# Design Documents

Module-level design docs for Heron's data pipeline. Ordered to follow the packet-to-metric flow.

For cross-cutting terminology (TTFT/E2E/TPOT, wire_api, agent_kind, HttpExchange, LlmCall, AgentTurn, …) see [../glossary.md](../glossary.md).

| # | Document | Crate | Description |
|---|----------|-------|-------------|
| 01 | [Architecture](01-architecture.md) | — | Monorepo layout, pipeline topology, crate dependency graph |
| 02 | [Capture](02-capture.md) | `h-capture` | libpcap + cloud-probe ZMQ packet acquisition, + eBPF SSL-uprobe on-host TLS capture (Linux) |
| 02b | [eBPF static targets](03-ebpf-static-targets.md) | `h-capture` | Byte-signature offset uprobes for static, symbol-stripped TLS (Bun / Claude Code) |
| 03 | [LLM](03-llm.md) | `ts-llm` | Wire-API detection, registry + extractor pattern |
| 04 | [Turn](04-turn.md) | `ts-turn` | Agent interaction (turn) grouping state machine |
| 05 | [Metrics](05-metrics.md) | `ts-metrics` | Sliding-window aggregation, t-digest percentiles |
| 06 | [Storage](06-storage.md) | `ts-storage` | Pluggable backend trait, write buffer, batch flush |
| 07 | [Schema](07-schema.md) | `ts-storage` | `agent_turns`, `llm_calls`, `llm_metrics` table definitions |
| 08 | [Internal Metrics](08-internal-metrics.md) | `ts-common` | Operational self-monitoring (counters, gauges) |
| 09 | [Body cap](09-body-cap.md) | `h-llm` / `h-common` | Stored-body head+tail sampling for 1M-token contexts |

## Pipeline Flow

```
capture → protocol (net + http) → llm → turn → metrics → storage
  02          (in 01)              03    04       05       06/07
```

## Conventions

- **Prefix** — `XX-` numeric prefix sets reading order (pipeline flow).
- **Scope** — Each doc covers one crate or cross-cutting concern.
- **Updates** — Keep docs in sync with code. If you change a crate's interface, update its design doc.
