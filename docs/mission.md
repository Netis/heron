# Heron Mission

## Mission

Heron extracts behavioral and business intelligence from LLM API traffic on the provider side — from passive packet evidence, no SDK required. It is Behavioral Packet Capture rebuilt for the AI era: the thing BPC always wanted to be, now tractable because LLM traffic is already structured intent, plan, and outcome.

## Vision

Heron becomes the open, vendor-neutral, protocol-level observer for LLM API traffic — the Wireshark of LLM observability. One passive capture, rich structured signals, consumable by any downstream tool: DuckDB-backed single-node deploys, ClickHouse-backed analytics clusters, OpenTelemetry pipelines, anyone's dashboard.

## Ambition

Every layer of LLM observability is in scope, from raw infra metrics up through business-outcome attribution. Discipline comes not from shrinking the scope but from walking the customer ladder — Ops foundations ship first, then Devs, then Dev-teams, then BU / Compliance / Procurement features that build on top.

## The BPC leap

Classical Behavioral Packet Capture tried to infer business behavior from enterprise network traffic. The methodology was sound; the payloads were opaque. BPC remained niche because the gap between "packets" and "business" was too wide for rules to cross.

LLM API traffic closes that gap. The payload is already the business substrate — the prompt is intent, the tool-call chain is plan, the response is outcome. AI-assisted analysis over that substrate delivers what BPC promised, without waking the request path.

Heron's architecture takes this seriously. L7 infra metrics are the floor, not the ceiling. Agent profiling, cost attribution, business-outcome correlation, and compliance signals are all layers on the same passive packet evidence.

## Who Heron is for

- **Ops — platform SREs, LLM provider infra teams, on-prem inference operators.** Keep inference clusters healthy. Tune prompt-cache and prefill-decode split from ground truth. Capacity plan from real traffic.

- **Devs — individual developers, agent builders, integrators.** Understand why an agent stalled or looped. Compare agent frameworks in production. Debug tool-call failures without touching the agent's code.

- **Dev-teams — engineering managers, FinOps leads, CTOs, platform engineering.** Attribute AI-assisted-development spend across projects, repos, teams, individuals, and models. Spot workloads running on oversized models. Enforce budgets without instrumenting every AI tool.

- **Business-unit owners.** Correlate LLM usage with business outcomes — resolution rate, NPS, conversion. Attribute cost to revenue-producing workloads.

- **Security and compliance.** Detect PII in prompts, monitor cross-border data flow via LLM calls, maintain the evidence chain for regulatory audit.

- **Procurement and vendor management.** Measure provider SLA conformance from the wire. Quantify outage impact. Back contract decisions with ground truth.

## What questions Heron answers

Heron is designed to answer questions at every layer of the stack. A selection, with the customer role most likely to ask:

**Infra ops and provider-side optimization** — Platform SRE, LLM provider:
- Is our inference cluster healthy?
- Is our prefill-decode split rational for this workload mix?
- Which clients drive our burst traffic?

**Agent behavioral profiling** — Developer, agent builder:
- Is Cursor stalling on tool calls versus its normal pattern?
- Does this custom agent loop more than baseline?
- Which agent framework is this session using?

**Dev-cost management** — Engineering manager, FinOps:
- Which team burns most Opus tokens for what output?
- What is our weekly AI-assisted-dev spend per repo?
- Who should move from premium to standard tier?

**Business-outcome attribution** — Product owner, BU leader:
- Did our support agent resolve the ticket?
- Which RAG queries returned low-value answers?

**Risk and compliance** — Security, compliance:
- Did any prompt leak customer PII to a third-party model?
- Is any workload crossing the EU/US data boundary?

**Vendor SLA and provider reliability** — Procurement, platform ops:
- Did the provider meet our SLA this quarter?
- What did the provider's outage cost us in downstream productivity?

**Model-portfolio optimization** — Engineering lead, FinOps:
- Which workloads can move from Opus to Haiku with no quality loss?
- Where are we over-buying frontier models for low-value queries?

**Conversation quality and loop detection** — Developer, agent builder:
- How often does the agent loop without progress?
- Which sessions stall at the same tool call repeatedly?

## See also

- [`README.md`](../README.md) — quick overview.
- [`docs/design/`](design/) — architecture and subsystem designs.
