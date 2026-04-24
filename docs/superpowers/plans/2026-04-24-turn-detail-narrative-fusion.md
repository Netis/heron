# Turn Detail Narrative Fusion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the Turn Detail narrative pane so that User Input fuses into Call#1, Final Answer fuses into Call#last, tool_use ↔ tool_result link by id across the whole turn, and packet-capture loss surfaces as explicit `⚠` states.

**Architecture:** Build a turn-scoped `Map<tool_use_id, {origin, resolution}>` from already-fetched call bodies, then use it to drive new `ToolUsePointer` / `ToolResultBackLink` components. Each call card gains labelled `Input · request body` / `Output · response body` subsections; Call#1 embeds `turn.user_input` in Input, Call#last embeds `turn.final_answer` in Output. The prior spec's Phase-1 shell (top bar, Gantt nav, raw-HTTP drawer) is unchanged.

**Tech Stack:** React 19 + TypeScript + Tailwind v4 + Bun (test runner: `bun:test`). Wire-api parsers already exist at `console/src/lib/wire-apis/{anthropic,openai-chat,openai-responses}`; this plan builds on them, no backend work.

**Spec:** [docs/superpowers/specs/2026-04-24-turn-detail-narrative-fusion-design.md](../specs/2026-04-24-turn-detail-narrative-fusion-design.md).

**Commands used in this plan:**
- Run tests: `cd console && bun test <path>` (or `just test ts <path>`)
- Lint + typecheck: `just quality ts`

---

## File Map

```
console/src/lib/turn-index.ts                         NEW — types, buildToolIndex, classifiers
console/src/lib/turn-index.test.ts                    NEW — unit tests for the above

console/src/components/turn-detail/
├── tool-pointer.tsx                                  NEW — ToolUsePointer + ToolResultBackLink
├── call-card.tsx                                     MODIFY — Input/Output subsections, first/last fusion
├── stats-cards.tsx                                   MODIFY — conditional Unresolved card
├── index.ts                                          MODIFY — drop UserCard / FinalAnswerCard exports
├── user-card.tsx                                     DELETE
└── final-answer-card.tsx                             DELETE

console/src/components/call-renderers/
├── dispatch.tsx                                      MODIFY — CallOutputDispatch + new CallInputDispatch
├── anthropic.tsx                                     MODIFY — pointer-based tool_use, new AnthropicInputBlocks
├── openai-chat.tsx                                   MODIFY — pointer on tool_calls, new OpenAiChatInputBlocks
└── openai-responses.tsx                              MODIFY — pointer on function_call items, new OpenAiResponsesInputBlocks

console/src/pages/agent-turn-detail-panel.tsx        MODIFY — build toolIndex, drop UserCard/FinalAnswerCard
```

---

## Task 1: Create `turn-index.ts` types and shell

**Files:**
- Create: `console/src/lib/turn-index.ts`
- Create: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing test**

```ts
// console/src/lib/turn-index.test.ts
import { describe, expect, it } from "bun:test"
import { buildToolIndex } from "./turn-index"

describe("buildToolIndex", () => {
  it("returns an empty map for an empty turn", () => {
    const index = buildToolIndex([])
    expect(index.size).toBe(0)
  })
})
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: FAIL — module `./turn-index` not found.

- [ ] **Step 3: Write minimal implementation**

```ts
// console/src/lib/turn-index.ts
import type { AgentTurnCallItem } from "@/types/api"

export interface ToolOrigin {
  call_sequence: number
  call_id: string
  tool_name: string
}

export interface ToolResolution {
  call_sequence: number
  call_id: string
  is_error: boolean
  size_bytes: number
  content: string
}

export interface ToolIndexEntry {
  origin: ToolOrigin | null
  resolution: ToolResolution | null
}

export type ToolIndex = Map<string, ToolIndexEntry>

export function buildToolIndex(_calls: AgentTurnCallItem[]): ToolIndex {
  return new Map()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS (1/1).

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/turn-index.ts console/src/lib/turn-index.test.ts
git commit -m "feat(console): add ToolIndex scaffolding for turn-scoped tool lookup"
```

---

## Task 2: Anthropic iterators + first-wins resolution

**Files:**
- Modify: `console/src/lib/turn-index.ts`
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing test**

Append to `console/src/lib/turn-index.test.ts`:

```ts
function anthropicCall(seq: number, id: string, body: {
  reqMsgs?: Array<{ role: "user" | "assistant"; content: Array<Record<string, unknown>> }>
  respContent?: Array<Record<string, unknown>>
} = {}) {
  return {
    id,
    sequence: seq,
    request_time: 0,
    response_time: null,
    complete_time: null,
    wire_api: "anthropic",
    model: "claude-sonnet-4-6",
    status_code: 200,
    is_stream: false,
    finish_reason: null,
    ttft_ms: null,
    e2e_latency_ms: null,
    input_tokens: null,
    output_tokens: null,
    request_path: "/v1/messages",
    client_ip: "",
    client_port: 0,
    server_ip: "",
    server_port: 0,
    request_body: body.reqMsgs ? JSON.stringify({ model: "x", messages: body.reqMsgs }) : null,
    response_body: body.respContent ? JSON.stringify({ content: body.respContent, stop_reason: "tool_use", usage: {} }) : null,
    request_headers: null,
    response_headers: null,
  } satisfies Parameters<typeof buildToolIndex>[0][number]
}

describe("buildToolIndex — anthropic", () => {
  it("matches tool_use in call#1 with tool_result in call#2", () => {
    const calls = [
      anthropicCall(1, "c1", {
        reqMsgs: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
        respContent: [{ type: "tool_use", id: "tu_01", name: "Read", input: { path: "a" } }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [
          { role: "user", content: [{ type: "text", text: "hi" }] },
          { role: "assistant", content: [{ type: "tool_use", id: "tu_01", name: "Read", input: { path: "a" } }] },
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_01", content: "ok", is_error: false }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toBe("ok")
  })
})
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: FAIL — `entry?.origin` is undefined.

- [ ] **Step 3: Write minimal implementation**

Replace the body of `buildToolIndex` in `console/src/lib/turn-index.ts`:

```ts
import type { AgentTurnCallItem } from "@/types/api"
import { parseAnthropicCall } from "./wire-apis/anthropic"

// (keep the exported interfaces from Task 1)

interface ToolUseBlock { id: string; name: string }
interface ToolResultBlock { tool_use_id: string; content: string; is_error: boolean }

function* iterAnthropicToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseAnthropicCall(null, responseBody)
  for (const block of call.response.content) {
    if (block.type === "tool_use") yield { id: block.id, name: block.name }
  }
}

function* iterAnthropicToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseAnthropicCall(requestBody, null)
  for (const msg of call.request.messages) {
    for (const block of msg.content) {
      if (block.type === "tool_result") {
        const content = typeof block.content === "string"
          ? block.content
          : JSON.stringify(block.content)
        yield { tool_use_id: block.tool_use_id, content, is_error: block.is_error }
      }
    }
  }
}

function byteLength(s: string): number {
  return new Blob([s]).size
}

export function buildToolIndex(calls: AgentTurnCallItem[]): ToolIndex {
  const index: ToolIndex = new Map()

  // Pass 1: tool_use origins (response side). First-wins — turn history is
  // carried forward in subsequent request bodies, but tool_use only appears
  // in the assistant response where it was first emitted.
  for (const call of calls) {
    if (call.wire_api !== "anthropic") continue
    for (const tu of iterAnthropicToolUses(call.response_body)) {
      if (index.has(tu.id)) continue
      index.set(tu.id, {
        origin: { call_sequence: call.sequence, call_id: call.id, tool_name: tu.name },
        resolution: null,
      })
    }
  }

  // Pass 2: tool_result resolutions (request side). First-wins — call#N+1's
  // request carries tool_results, and so does every subsequent call's history.
  // Record the earliest call that carried each result.
  for (const call of calls) {
    if (call.wire_api !== "anthropic") continue
    for (const tr of iterAnthropicToolResults(call.request_body)) {
      const existing = index.get(tr.tool_use_id)
      if (existing?.resolution) continue
      const entry = existing ?? { origin: null, resolution: null }
      entry.resolution = {
        call_sequence: call.sequence,
        call_id: call.id,
        is_error: tr.is_error,
        size_bytes: byteLength(tr.content),
        content: tr.content,
      }
      index.set(tr.tool_use_id, entry)
    }
  }

  return index
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS (2/2).

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/turn-index.ts console/src/lib/turn-index.test.ts
git commit -m "feat(console): build anthropic ToolIndex with first-wins dedup"
```

---

## Task 3: OpenAI-chat iterators

**Files:**
- Modify: `console/src/lib/turn-index.ts`
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing test**

Append to `console/src/lib/turn-index.test.ts`:

```ts
describe("buildToolIndex — openai-chat", () => {
  it("matches tool_calls[].id with role=tool messages by tool_call_id", () => {
    const c1 = {
      id: "c1", sequence: 1, wire_api: "openai-chat", model: "gpt-4",
      request_time: 0, response_time: null, complete_time: null,
      status_code: 200, is_stream: false, finish_reason: null,
      ttft_ms: null, e2e_latency_ms: null, input_tokens: null, output_tokens: null,
      request_path: "/v1/chat/completions", client_ip: "", client_port: 0, server_ip: "", server_port: 0,
      request_body: JSON.stringify({ model: "gpt-4", messages: [{ role: "user", content: "hi" }] }),
      response_body: JSON.stringify({
        choices: [{
          index: 0, finish_reason: "tool_calls",
          message: {
            role: "assistant", content: null,
            tool_calls: [{ id: "call_01", type: "function", function: { name: "Read", arguments: "{\"p\":1}" } }],
          },
        }],
      }),
      request_headers: null, response_headers: null,
    }
    const c2 = {
      ...c1, id: "c2", sequence: 2,
      request_body: JSON.stringify({
        model: "gpt-4",
        messages: [
          { role: "user", content: "hi" },
          { role: "assistant", content: null, tool_calls: [{ id: "call_01", type: "function", function: { name: "Read", arguments: "{\"p\":1}" } }] },
          { role: "tool", tool_call_id: "call_01", content: "ok" },
        ],
      }),
      response_body: null,
    }
    const index = buildToolIndex([c1, c2] as any)
    const entry = index.get("call_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toBe("ok")
  })
})
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: FAIL — `call_01` not in index (we haven't added the openai-chat iterators yet).

- [ ] **Step 3: Write minimal implementation**

Add to `console/src/lib/turn-index.ts`:

```ts
import { parseOpenAiChatCall } from "./wire-apis/openai-chat"

function* iterOpenAiChatToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseOpenAiChatCall(null, responseBody)
  for (const choice of call.response.choices) {
    for (const tc of choice.message.tool_calls ?? []) {
      yield { id: tc.id, name: tc.function.name }
    }
  }
}

function* iterOpenAiChatToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseOpenAiChatCall(requestBody, null)
  for (const msg of call.request.messages) {
    if (msg.role === "tool" && msg.tool_call_id) {
      const content = typeof msg.content === "string"
        ? msg.content
        : JSON.stringify(msg.content ?? "")
      yield { tool_use_id: msg.tool_call_id, content, is_error: false }
    }
  }
}
```

Wire into `buildToolIndex` by adding an `openai-chat` branch to each pass:

```ts
// inside Pass 1
} else if (call.wire_api === "openai-chat") {
  for (const tu of iterOpenAiChatToolUses(call.response_body)) {
    if (index.has(tu.id)) continue
    index.set(tu.id, { origin: { call_sequence: call.sequence, call_id: call.id, tool_name: tu.name }, resolution: null })
  }
}

// inside Pass 2
} else if (call.wire_api === "openai-chat") {
  for (const tr of iterOpenAiChatToolResults(call.request_body)) {
    const existing = index.get(tr.tool_use_id)
    if (existing?.resolution) continue
    const entry = existing ?? { origin: null, resolution: null }
    entry.resolution = { call_sequence: call.sequence, call_id: call.id, is_error: tr.is_error, size_bytes: byteLength(tr.content), content: tr.content }
    index.set(tr.tool_use_id, entry)
  }
}
```

Refactor the per-pass branching into a small helper so the two passes don't grow copy-paste. Propose:

```ts
function* iterToolUses(call: AgentTurnCallItem): Generator<ToolUseBlock> {
  switch (call.wire_api) {
    case "anthropic":       yield* iterAnthropicToolUses(call.response_body); break
    case "openai-chat":     yield* iterOpenAiChatToolUses(call.response_body); break
  }
}
function* iterToolResults(call: AgentTurnCallItem): Generator<ToolResultBlock> {
  switch (call.wire_api) {
    case "anthropic":       yield* iterAnthropicToolResults(call.request_body); break
    case "openai-chat":     yield* iterOpenAiChatToolResults(call.request_body); break
  }
}
```

Collapse both passes in `buildToolIndex` to use these dispatch generators.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS (3/3).

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/turn-index.ts console/src/lib/turn-index.test.ts
git commit -m "feat(console): add openai-chat iterators to ToolIndex"
```

---

## Task 4: OpenAI-responses iterators

**Files:**
- Modify: `console/src/lib/turn-index.ts`
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing test**

Append to `console/src/lib/turn-index.test.ts`:

```ts
describe("buildToolIndex — openai-responses", () => {
  it("matches function_call.call_id with function_call_output.call_id", () => {
    const c1 = {
      id: "c1", sequence: 1, wire_api: "openai-responses", model: "gpt-5",
      request_time: 0, response_time: null, complete_time: null,
      status_code: 200, is_stream: false, finish_reason: null,
      ttft_ms: null, e2e_latency_ms: null, input_tokens: null, output_tokens: null,
      request_path: "/v1/responses", client_ip: "", client_port: 0, server_ip: "", server_port: 0,
      request_body: JSON.stringify({ model: "gpt-5", input: [{ type: "message", role: "user", content: [{ type: "input_text", text: "hi" }] }] }),
      response_body: JSON.stringify({
        output: [{ type: "function_call", call_id: "fc_01", name: "Read", arguments: "{\"p\":1}" }],
      }),
      request_headers: null, response_headers: null,
    }
    const c2 = {
      ...c1, id: "c2", sequence: 2,
      request_body: JSON.stringify({
        model: "gpt-5",
        input: [
          { type: "message", role: "user", content: [{ type: "input_text", text: "hi" }] },
          { type: "function_call", call_id: "fc_01", name: "Read", arguments: "{\"p\":1}" },
          { type: "function_call_output", call_id: "fc_01", output: "ok" },
        ],
      }),
      response_body: null,
    }
    const index = buildToolIndex([c1, c2] as any)
    const entry = index.get("fc_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toContain("ok")
  })
})
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: FAIL — `fc_01` not in index.

- [ ] **Step 3: Write minimal implementation**

Add to `console/src/lib/turn-index.ts`:

```ts
import { parseOpenAiResponsesCall } from "./wire-apis/openai-responses"

function* iterOpenAiResponsesToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseOpenAiResponsesCall(null, responseBody)
  for (const item of call.response.output) {
    if (item.kind === "function_call") {
      yield { id: item.call_id, name: item.name }
    }
  }
}

function* iterOpenAiResponsesToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseOpenAiResponsesCall(requestBody, null)
  for (const item of call.request.input) {
    if (item.kind === "function_call_output") {
      const content = typeof item.output === "string" ? item.output : JSON.stringify(item.output)
      yield { tool_use_id: item.call_id, content, is_error: false }
    }
  }
}
```

Add `"openai-responses"` branches to `iterToolUses` / `iterToolResults` dispatchers.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS (4/4).

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/turn-index.ts console/src/lib/turn-index.test.ts
git commit -m "feat(console): add openai-responses iterators to ToolIndex"
```

---

## Task 5: Orphan + middle-gap coverage

**Files:**
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing tests**

Append:

```ts
describe("buildToolIndex — capture loss", () => {
  it("records null resolution when tool_use has no matching tool_result anywhere", () => {
    const calls = [
      anthropicCall(1, "c1", {
        respContent: [{ type: "tool_use", id: "tu_gap", name: "Read", input: {} }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [{ role: "user", content: [{ type: "text", text: "continue" }] }],
        respContent: [{ type: "text", text: "done" }],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_gap")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.resolution).toBeNull()
  })

  it("records null origin when tool_result has no matching tool_use (orphan)", () => {
    const calls = [
      anthropicCall(1, "c1", {
        reqMsgs: [
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_orphan", content: "stray", is_error: false }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_orphan")
    expect(entry?.origin).toBeNull()
    expect(entry?.resolution?.call_sequence).toBe(1)
  })

  it("first-wins: tool_result appearing in call#2 and #3 history records #2", () => {
    const tr = { type: "tool_result", tool_use_id: "tu_first", content: "v1", is_error: false }
    const calls = [
      anthropicCall(1, "c1", {
        respContent: [{ type: "tool_use", id: "tu_first", name: "Read", input: {} }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [{ role: "user", content: [tr] }],
        respContent: [{ type: "text", text: "ack" }],
      }),
      anthropicCall(3, "c3", {
        // #3 carries the full history, including tr
        reqMsgs: [
          { role: "user", content: [tr] },
          { role: "assistant", content: [{ type: "text", text: "ack" }] },
          { role: "user", content: [{ type: "text", text: "continue" }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    expect(index.get("tu_first")?.resolution?.call_sequence).toBe(2)
  })
})
```

- [ ] **Step 2: Run tests**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS — Task 2/3's implementation already handles these cases correctly (the first-wins dedup + independent origin/resolution population covers them). If any test fails, fix `buildToolIndex` until green.

- [ ] **Step 3: Commit**

```bash
git add console/src/lib/turn-index.test.ts
git commit -m "test(console): cover orphan tool_result and mid-turn capture gaps"
```

---

## Task 6: Pointer state classifiers

**Files:**
- Modify: `console/src/lib/turn-index.ts`
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Write the failing test**

Append:

```ts
import { classifyToolUseState, classifyToolResultState, type ToolUseState } from "./turn-index"

const LEGIT_END_REASONS = ["end_turn", "stop", "max_tokens", "stop_sequence"]

describe("classifyToolUseState", () => {
  const mkTurn = (opts: { final_call_id?: string | null; final_finish_reason?: string | null } = {}) => ({
    final_call_id: opts.final_call_id ?? null,
    final_finish_reason: opts.final_finish_reason ?? null,
  })

  it("healthy when resolution is set", () => {
    const state = classifyToolUseState(
      { origin: null, resolution: { call_sequence: 2, call_id: "c2", is_error: false, size_bytes: 0, content: "" } },
      { isFinalCall: false, turn: mkTurn() },
    )
    expect(state).toBe<ToolUseState>("healthy")
  })

  it("legit_pending when no resolution AND final call AND normal finish_reason", () => {
    const state = classifyToolUseState(
      { origin: null, resolution: null },
      { isFinalCall: true, turn: mkTurn({ final_call_id: "c5", final_finish_reason: "end_turn" }) },
    )
    expect(state).toBe<ToolUseState>("legit_pending")
  })

  it("capture_gap when no resolution AND not final call", () => {
    const state = classifyToolUseState(
      { origin: null, resolution: null },
      { isFinalCall: false, turn: mkTurn() },
    )
    expect(state).toBe<ToolUseState>("capture_gap")
  })

  it("capture_gap when final call but finish_reason is null (abnormal end)", () => {
    const state = classifyToolUseState(
      { origin: null, resolution: null },
      { isFinalCall: true, turn: mkTurn({ final_call_id: "c5", final_finish_reason: null }) },
    )
    expect(state).toBe<ToolUseState>("capture_gap")
  })
})

describe("classifyToolResultState", () => {
  it("healthy when origin is set", () => {
    expect(classifyToolResultState({
      origin: { call_sequence: 1, call_id: "c1", tool_name: "Read" },
      resolution: null,
    })).toBe("healthy")
  })

  it("orphan when origin is null", () => {
    expect(classifyToolResultState({ origin: null, resolution: null })).toBe("orphan")
  })
})
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `classifyToolUseState` / `classifyToolResultState` not exported.

- [ ] **Step 3: Write minimal implementation**

Append to `console/src/lib/turn-index.ts`:

```ts
export type ToolUseState = "healthy" | "legit_pending" | "capture_gap"
export type ToolResultState = "healthy" | "orphan"

const LEGIT_END_REASONS = new Set(["end_turn", "stop", "max_tokens", "stop_sequence"])

export interface TurnForClassification {
  final_call_id: string | null
  final_finish_reason: string | null
}

export function classifyToolUseState(
  entry: ToolIndexEntry,
  ctx: { isFinalCall: boolean; turn: TurnForClassification },
): ToolUseState {
  if (entry.resolution != null) return "healthy"
  if (ctx.isFinalCall && ctx.turn.final_finish_reason != null && LEGIT_END_REASONS.has(ctx.turn.final_finish_reason)) {
    return "legit_pending"
  }
  return "capture_gap"
}

export function classifyToolResultState(entry: ToolIndexEntry): ToolResultState {
  return entry.origin == null ? "orphan" : "healthy"
}

export function countUnresolved(index: ToolIndex, turn: TurnForClassification, finalCallId: string | null): number {
  let count = 0
  for (const entry of index.values()) {
    if (entry.resolution == null) {
      const isFinal = entry.origin?.call_id === finalCallId
      if (classifyToolUseState(entry, { isFinalCall: isFinal, turn }) === "capture_gap") count++
    }
    if (entry.origin == null) count++  // orphan results
  }
  return count
}
```

Add one more test for `countUnresolved`:

```ts
describe("countUnresolved", () => {
  it("counts capture-gap tool_uses and orphan tool_results, not legit pending", () => {
    const index: ToolIndex = new Map([
      ["healthy", { origin: { call_sequence: 1, call_id: "c1", tool_name: "A" }, resolution: { call_sequence: 2, call_id: "c2", is_error: false, size_bytes: 1, content: "x" } }],
      ["gap",     { origin: { call_sequence: 1, call_id: "c1", tool_name: "B" }, resolution: null }],
      ["pending", { origin: { call_sequence: 3, call_id: "c3", tool_name: "C" }, resolution: null }],
      ["orphan",  { origin: null, resolution: { call_sequence: 2, call_id: "c2", is_error: false, size_bytes: 1, content: "x" } }],
    ])
    expect(countUnresolved(index, { final_call_id: "c3", final_finish_reason: "end_turn" }, "c3")).toBe(2)
  })
})
```

- [ ] **Step 4: Run tests**

Run: `cd console && bun test src/lib/turn-index.test.ts`
Expected: PASS — all classifier + counter tests green.

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/turn-index.ts console/src/lib/turn-index.test.ts
git commit -m "feat(console): add tool pointer state classifiers and unresolved counter"
```

---

## Task 7: Shared pointer components

**Files:**
- Create: `console/src/components/turn-detail/tool-pointer.tsx`
- Modify: `console/src/components/turn-detail/index.ts`

- [ ] **Step 1: Write the component**

```tsx
// console/src/components/turn-detail/tool-pointer.tsx
import { AlertTriangle } from "lucide-react"
import { cn } from "@/lib/utils"
import type { ToolUseState, ToolResultState } from "@/lib/turn-index"

interface ToolUsePointerProps {
  state: ToolUseState
  resolvedInSeq: number | null
  onJump?: (sequence: number) => void
  className?: string
}

export function ToolUsePointer({ state, resolvedInSeq, onJump, className }: ToolUsePointerProps) {
  if (state === "healthy" && resolvedInSeq != null) {
    return (
      <button
        type="button"
        onClick={() => onJump?.(resolvedInSeq)}
        className={cn("text-[11px] text-blue-700 hover:underline dark:text-blue-400", className)}
      >
        → result in #{resolvedInSeq} ✓
      </button>
    )
  }
  if (state === "legit_pending") {
    return <span className={cn("text-[11px] text-muted-foreground", className)}>→ no response (turn ended)</span>
  }
  return (
    <span className={cn("inline-flex items-center gap-1 text-[11px] font-medium text-amber-700 dark:text-amber-400", className)}>
      <AlertTriangle className="size-3" />
      → result not captured
    </span>
  )
}

interface ToolResultBackLinkProps {
  state: ToolResultState
  originSeq: number | null
  originToolName: string | null
  onJump?: (sequence: number) => void
  className?: string
}

export function ToolResultBackLink({ state, originSeq, originToolName, onJump, className }: ToolResultBackLinkProps) {
  if (state === "healthy" && originSeq != null) {
    return (
      <button
        type="button"
        onClick={() => onJump?.(originSeq)}
        className={cn("text-[11px] text-blue-700 hover:underline dark:text-blue-400", className)}
      >
        ← from #{originSeq}{originToolName ? ` · ${originToolName}` : ""}
      </button>
    )
  }
  return (
    <span className={cn("inline-flex items-center gap-1 text-[11px] font-medium text-amber-700 dark:text-amber-400", className)}>
      <AlertTriangle className="size-3" />
      ← origin not captured
    </span>
  )
}
```

- [ ] **Step 2: Export from the turn-detail barrel**

Edit `console/src/components/turn-detail/index.ts`:

```ts
export { TopBar } from "./top-bar"
export { StatsCards } from "./stats-cards"
export { GanttNav } from "./gantt-nav"
export { UserCard } from "./user-card"
export { FinalAnswerCard } from "./final-answer-card"
export { CallCard } from "./call-card"
export { RawHttpDrawer } from "./raw-http-drawer"
export { MetadataPopover } from "./metadata-popover"
export { ToolUsePointer, ToolResultBackLink } from "./tool-pointer"
```

- [ ] **Step 3: Typecheck**

Run: `just quality ts`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add console/src/components/turn-detail/tool-pointer.tsx console/src/components/turn-detail/index.ts
git commit -m "feat(console): add ToolUsePointer and ToolResultBackLink components"
```

---

## Task 8: Refactor `dispatch.tsx` — add CallInputDispatch, swap props to ToolIndex

**Files:**
- Modify: `console/src/components/call-renderers/dispatch.tsx`

This task changes the dispatch signature but does NOT yet rewire call-card.tsx; Task 9+ will migrate the renderers and the call site together. The dispatch file compiles standalone.

- [ ] **Step 1: Replace the file content**

Overwrite `console/src/components/call-renderers/dispatch.tsx` (the existing `CallOutputDispatch` signature must change — there is no back-compat need since the only caller is `CallCard`, updated in Task 13):

```tsx
import { AnthropicCallView, AnthropicOutputBlocks, AnthropicInputBlocks, anthropicParseForOutput, anthropicParseForInput } from "./anthropic"
import { OpenAiChatCallView, OpenAiChatOutputBlocks, OpenAiChatInputBlocks, openaiChatParseForOutput, openaiChatParseForInput } from "./openai-chat"
import { OpenAiResponsesCallView, OpenAiResponsesOutputBlocks, OpenAiResponsesInputBlocks, openaiResponsesParseForOutput, openaiResponsesParseForInput } from "./openai-responses"
import { RawJsonFallback } from "./fallback"
import { ClaudeCliOverlay } from "./overlays/claude-cli"
import type { CallOverlay } from "./overlays/types"
import type { ToolIndex, TurnForClassification } from "@/lib/turn-index"

const agentOverlays: Record<string, CallOverlay> = {
  "claude-cli": ClaudeCliOverlay,
}

function overlayFor(agentKind: string | null): CallOverlay | null {
  if (!agentKind) return null
  return agentOverlays[agentKind] ?? null
}

// ── full detail view (raw HTTP drawer) — unchanged signature ──────────────
export interface CallRendererDispatchProps {
  wireApi: string
  agentKind?: string | null
  requestBody: string | null
  responseBody: string | null
  hasRequestBody: boolean
}

export function CallRendererDispatch(props: CallRendererDispatchProps) {
  const overlay = overlayFor(props.agentKind ?? null)
  switch (props.wireApi) {
    case "anthropic":
      return <AnthropicCallView requestBody={props.requestBody} responseBody={props.responseBody} overlay={overlay} hasRequestBody={props.hasRequestBody} />
    case "openai-chat":
      return <OpenAiChatCallView requestBody={props.requestBody} responseBody={props.responseBody} overlay={overlay} hasRequestBody={props.hasRequestBody} />
    case "openai-responses":
      return <OpenAiResponsesCallView requestBody={props.requestBody} responseBody={props.responseBody} overlay={overlay} hasRequestBody={props.hasRequestBody} />
    default:
      return <RawJsonFallback wireApi={props.wireApi} requestBody={props.requestBody} responseBody={props.responseBody} hasRequestBody={props.hasRequestBody} />
  }
}

// ── Output subsection (inside CallCard expanded) ──────────────────────────
export interface CallOutputDispatchProps {
  wireApi: string
  agentKind: string | null
  responseBody: string | null
  toolIndex: ToolIndex
  callId: string
  finalCallId: string | null
  turn: TurnForClassification
  onJump?: (sequence: number) => void
}

export function CallOutputDispatch(props: CallOutputDispatchProps) {
  const overlay = overlayFor(props.agentKind)
  const ctx = {
    toolIndex: props.toolIndex,
    callId: props.callId,
    isFinalCall: props.callId === props.finalCallId,
    turn: props.turn,
    onJump: props.onJump,
  }
  switch (props.wireApi) {
    case "anthropic": {
      const { response } = anthropicParseForOutput(null, props.responseBody)
      return <AnthropicOutputBlocks response={response} overlay={overlay} ctx={ctx} />
    }
    case "openai-chat": {
      const { response } = openaiChatParseForOutput(null, props.responseBody)
      return <OpenAiChatOutputBlocks response={response} ctx={ctx} />
    }
    case "openai-responses": {
      const { response } = openaiResponsesParseForOutput(null, props.responseBody)
      return <OpenAiResponsesOutputBlocks response={response} overlay={overlay} ctx={ctx} />
    }
    default:
      return (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-[11px] text-muted-foreground">
          No output renderer for wire_api "{props.wireApi}". Open raw HTTP for details.
        </div>
      )
  }
}

// ── Input subsection (inside CallCard expanded, non-first calls) ──────────
export interface CallInputDispatchProps {
  wireApi: string
  agentKind: string | null
  requestBody: string | null
  toolIndex: ToolIndex
  onJump?: (sequence: number) => void
}

export function CallInputDispatch(props: CallInputDispatchProps) {
  const overlay = overlayFor(props.agentKind)
  const ctx = { toolIndex: props.toolIndex, onJump: props.onJump }
  switch (props.wireApi) {
    case "anthropic":
      return <AnthropicInputBlocks parsed={anthropicParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    case "openai-chat":
      return <OpenAiChatInputBlocks parsed={openaiChatParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    case "openai-responses":
      return <OpenAiResponsesInputBlocks parsed={openaiResponsesParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    default:
      return null
  }
}
```

This will not compile yet — the new exports (`AnthropicInputBlocks`, `anthropicParseForInput`, analogous for openai-chat and openai-responses, plus the `ctx` prop on each Output renderer) are added in Tasks 9/10/11.

- [ ] **Step 2: Do NOT commit yet**

The file is intentionally broken between tasks. Proceed directly to Task 9.

---

## Task 9: Migrate Anthropic renderer to ToolIndex + add AnthropicInputBlocks

**Files:**
- Modify: `console/src/components/call-renderers/anthropic.tsx`

The rewrite:
1. Drop `buildResultLookup` and the `nextCallRequestBody` prop.
2. Change `AnthropicOutputBlocks` to take a `ctx: OutputCtx` instead of `resultLookup`.
3. Inside `ToolUseBlockView`, replace the inline `⤷ result` details with a `ToolUsePointer` (the jump target).
4. Add a new `AnthropicInputBlocks` component that renders the tool_result blocks of a parsed request as grey cards with `ToolResultBackLink` back-pointers.
5. Add helper `anthropicParseForInput(requestBody)` → `{ toolResults: Array<{ tool_use_id, content, is_error }>, otherBlocks: ... }`.

- [ ] **Step 1: Add the new OutputCtx type at the top of the file**

Replace the existing `ToolResultLookup` section:

```tsx
import { ToolUsePointer, ToolResultBackLink } from "@/components/turn-detail/tool-pointer"
import { classifyToolUseState, classifyToolResultState, type ToolIndex, type TurnForClassification } from "@/lib/turn-index"

interface OutputCtx {
  toolIndex: ToolIndex
  callId: string
  isFinalCall: boolean
  turn: TurnForClassification
  onJump?: (sequence: number) => void
}

interface InputCtx {
  toolIndex: ToolIndex
  onJump?: (sequence: number) => void
}
```

Delete the `ToolResultLookup` type alias and the `buildResultLookup` function.

- [ ] **Step 2: Rewrite `ToolUseBlockView` to use the pointer**

Replace the existing `ToolUseBlockView` body with:

```tsx
function ToolUseBlockView({
  id,
  name,
  input,
  ctx,
}: {
  id: string
  name: string
  input: unknown
  ctx: OutputCtx
}) {
  const [argsOpen, setArgsOpen] = useState(true)
  const entry = ctx.toolIndex.get(id) ?? { origin: null, resolution: null }
  const state = classifyToolUseState(entry, { isFinalCall: ctx.isFinalCall, turn: ctx.turn })
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">🔧 {name}</span>
        <span className="font-mono text-[10px] text-muted-foreground">{id}</span>
      </div>
      <details className="mt-1" open={argsOpen} onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">input</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(input)}
        </pre>
      </details>
      <div className="mt-1">
        <ToolUsePointer state={state} resolvedInSeq={entry.resolution?.call_sequence ?? null} onJump={ctx.onJump} />
      </div>
    </div>
  )
}
```

- [ ] **Step 3: Add `AnthropicInputBlocks` + `anthropicParseForInput`**

Append to `anthropic.tsx`:

```tsx
export interface AnthropicParsedInput {
  toolResults: Array<{
    tool_use_id: string
    content: string
    is_error: boolean
  }>
  extraUserText: string | null  // rare: new user-role text that isn't a tool_result
}

export function anthropicParseForInput(requestBody: string | null | undefined): AnthropicParsedInput {
  if (!requestBody) return { toolResults: [], extraUserText: null }
  const call = parseAnthropicCall(requestBody, null)
  // Take only the last user-role message's content — that's the delta from the prior call.
  const lastUserMsg = [...call.request.messages].reverse().find((m) => m.role === "user")
  if (!lastUserMsg) return { toolResults: [], extraUserText: null }
  const toolResults: AnthropicParsedInput["toolResults"] = []
  let extraUserText: string | null = null
  for (const block of lastUserMsg.content) {
    if (block.type === "tool_result") {
      const content = typeof block.content === "string" ? block.content : formatJson(block.content)
      toolResults.push({ tool_use_id: block.tool_use_id, content, is_error: block.is_error })
    } else if (block.type === "text") {
      extraUserText = (extraUserText ?? "") + (extraUserText ? "\n\n" : "") + block.text
    }
  }
  return { toolResults, extraUserText }
}

export function AnthropicInputBlocks({
  parsed,
  ctx,
  overlay,
}: {
  parsed: AnthropicParsedInput
  ctx: InputCtx
  overlay?: CallOverlay | null
}) {
  const ToolResult = overlay?.ToolResultContent
  if (parsed.toolResults.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.toolResults.map((tr) => {
        const entry = ctx.toolIndex.get(tr.tool_use_id) ?? { origin: null, resolution: null }
        const state = classifyToolResultState(entry)
        const errored = tr.is_error
        return (
          <div
            key={tr.tool_use_id}
            className={cn(
              "rounded border p-2 text-[11px]",
              errored
                ? "bg-red-50 border-red-200 dark:bg-red-900/10 dark:border-red-900/40"
                : state === "orphan"
                  ? "bg-amber-50/60 border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40"
                  : "bg-muted/40 border-border/60",
            )}
          >
            <div className="flex items-center gap-2">
              <span className={cn("font-medium", errored && "text-red-700 dark:text-red-400")}>
                ⤷ {errored ? "error" : "tool_result"}
              </span>
              <span className="font-mono text-[10px] text-muted-foreground">{tr.tool_use_id}</span>
              <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(tr.content))}</span>
              <ToolResultBackLink
                state={state}
                originSeq={entry.origin?.call_sequence ?? null}
                originToolName={entry.origin?.tool_name ?? null}
                onJump={ctx.onJump}
                className="ml-auto"
              />
            </div>
            <div className="mt-1">
              {ToolResult
                ? <ToolResult content={tr.content} isError={errored} />
                : <pre className={cn("max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]", errored && "text-red-700 dark:text-red-400")}>{tr.content}</pre>}
            </div>
          </div>
        )
      })}
      {parsed.extraUserText && (
        <div className="rounded border border-blue-200 bg-blue-50/60 p-3 text-[11px] dark:border-blue-900/40 dark:bg-blue-900/10">
          <Markdown text={parsed.extraUserText} />
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 4: Update the old `AnthropicOutputBlocks` signature**

Replace the old `resultLookup`-based signature with ctx-based:

```tsx
export function AnthropicOutputBlocks({
  response,
  ctx,
  overlay,
}: {
  response: AnthropicResponse
  ctx: OutputCtx
  overlay?: CallOverlay | null
}) {
  if (response.content.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response content.</div>
  }
  return (
    <div className="space-y-2">
      {response.content.map((b, i) => (
        <BlockView key={i} block={b} ctx={ctx} overlay={overlay} />
      ))}
    </div>
  )
}
```

Update `BlockView` to accept `ctx` and pass it to `ToolUseBlockView`. Remove all `tool_result` rendering inside `BlockView` (tool_results are no longer inlined into Output — they live in Input now) — replace that case with:

```tsx
case "tool_result":
  // Should not appear in response.content; ignore defensively.
  return null
```

Update `AnthropicCallView` (the full detail view shown in the raw-HTTP drawer) to construct a minimal pass-through `ctx` — since the raw-HTTP drawer doesn't have a turn context, we make the pointer degrade gracefully by using an empty index and a `TurnForClassification` with nulls. The pointer rendering will show `⚠ result not captured` for every tool_use in the drawer view, which is consistent (the drawer isolates a single call; cross-call resolution isn't meaningful there). Alternative: keep `resultLookup` alive for this specific entry point. We opt for the simpler path: pass `ctx` with empty index.

In `AnthropicCallView`, inside the render, change:

```tsx
const resultLookup = useMemo(() => buildResultLookup(call, nextCallRequestBody), [call, nextCallRequestBody])
```

to:

```tsx
const drawerCtx: OutputCtx = {
  toolIndex: new Map(),
  callId: "",
  isFinalCall: false,
  turn: { final_call_id: null, final_finish_reason: null },
}
```

And replace the `<AnthropicOutputBlocks response={call.response} resultLookup={resultLookup} overlay={overlay} />` JSX with:

```tsx
<AnthropicOutputBlocks response={call.response} ctx={drawerCtx} overlay={overlay} />
```

Remove the `nextCallRequestBody` prop from `AnthropicCallViewProps` and the usage site (dispatch.tsx Task 8 already reflects this).

Finally, also remove the now-dead `anthropicParseForOutput` third arg and the `resultLookup` return value:

```tsx
export function anthropicParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
) {
  const call = parseAnthropicCall(requestBody, responseBody)
  return { response: call.response }
}
```

- [ ] **Step 5: Run lint/typecheck**

Run: `just quality ts`
Expected: the `anthropic.tsx` file typechecks. Other wire-api files will still fail until Tasks 10/11.

- [ ] **Step 6: Do NOT commit yet**

Continue to Task 10.

---

## Task 10: Migrate OpenAI-chat renderer + add OpenAiChatInputBlocks

**Files:**
- Modify: `console/src/components/call-renderers/openai-chat.tsx`

- [ ] **Step 1: Add OutputCtx/InputCtx + imports**

At the top of `openai-chat.tsx`:

```tsx
import { ToolUsePointer, ToolResultBackLink } from "@/components/turn-detail/tool-pointer"
import { classifyToolUseState, classifyToolResultState, type ToolIndex, type TurnForClassification } from "@/lib/turn-index"

interface OutputCtx {
  toolIndex: ToolIndex
  callId: string
  isFinalCall: boolean
  turn: TurnForClassification
  onJump?: (sequence: number) => void
}
interface InputCtx {
  toolIndex: ToolIndex
  onJump?: (sequence: number) => void
}
```

- [ ] **Step 2: Rewrite `ToolCallView` to take ctx and append the pointer**

```tsx
function ToolCallView({ tc, ctx }: { tc: OpenAiChatToolCall; ctx?: OutputCtx }) {
  const [open, setOpen] = useState(true)
  const entry = ctx?.toolIndex.get(tc.id) ?? { origin: null, resolution: null }
  const state = ctx ? classifyToolUseState(entry, { isFinalCall: ctx.isFinalCall, turn: ctx.turn }) : "healthy"
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">🔧 {tc.function.name}</span>
        <span className="font-mono text-[10px] text-muted-foreground">{tc.id}</span>
      </div>
      <details className="mt-1" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">arguments</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(safeParseJson(tc.function.arguments) ?? tc.function.arguments)}
        </pre>
      </details>
      {ctx && (
        <div className="mt-1">
          <ToolUsePointer state={state} resolvedInSeq={entry.resolution?.call_sequence ?? null} onJump={ctx.onJump} />
        </div>
      )}
    </div>
  )
}
```

(The `ctx?` / fallback is so the drawer-level `OpenAiChatCallView` can continue calling `ToolCallView` without ctx.)

- [ ] **Step 3: Thread ctx through `ResponseCard` and `ChoiceCard`**

Change `ResponseCard({ response })` and `ChoiceCard({ choice })` signatures to accept `ctx?: OutputCtx`. Where they pass tool_calls to `ToolCallView`, include the ctx prop.

- [ ] **Step 4: Update `OpenAiChatOutputBlocks` signature**

```tsx
export function OpenAiChatOutputBlocks({
  response,
  ctx,
}: {
  response: OpenAiChatResponse
  ctx: OutputCtx
}) {
  if (response.choices.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response content.</div>
  }
  return (
    <div className="space-y-2">
      {response.choices.map((c, i) => <ChoiceCard key={i} choice={c} ctx={ctx} />)}
    </div>
  )
}
```

- [ ] **Step 5: Add `OpenAiChatInputBlocks` + `openaiChatParseForInput`**

Append:

```tsx
export interface OpenAiChatParsedInput {
  toolResults: Array<{ tool_call_id: string; content: string }>
  extraUserText: string | null
}

export function openaiChatParseForInput(requestBody: string | null | undefined): OpenAiChatParsedInput {
  if (!requestBody) return { toolResults: [], extraUserText: null }
  const call = parseOpenAiChatCall(requestBody, null)
  // Walk the tail of messages — everything that follows the last assistant message
  // represents the new turn delta (tool results, and optionally a new user message).
  const msgs = call.request.messages
  let lastAssistantIdx = -1
  for (let i = msgs.length - 1; i >= 0; i--) {
    if (msgs[i].role === "assistant") { lastAssistantIdx = i; break }
  }
  const tail = msgs.slice(lastAssistantIdx + 1)
  const toolResults: OpenAiChatParsedInput["toolResults"] = []
  let extraUserText: string | null = null
  for (const m of tail) {
    if (m.role === "tool" && m.tool_call_id) {
      const content = typeof m.content === "string" ? m.content : formatJson(m.content ?? "")
      toolResults.push({ tool_call_id: m.tool_call_id, content })
    } else if (m.role === "user" && typeof m.content === "string") {
      extraUserText = m.content
    }
  }
  return { toolResults, extraUserText }
}

export function OpenAiChatInputBlocks({
  parsed,
  ctx,
}: {
  parsed: OpenAiChatParsedInput
  ctx: InputCtx
  overlay?: CallOverlay | null
}) {
  if (parsed.toolResults.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.toolResults.map((tr) => {
        const entry = ctx.toolIndex.get(tr.tool_call_id) ?? { origin: null, resolution: null }
        const state = classifyToolResultState(entry)
        return (
          <div
            key={tr.tool_call_id}
            className={cn(
              "rounded border p-2 text-[11px]",
              state === "orphan"
                ? "bg-amber-50/60 border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40"
                : "bg-muted/40 border-border/60",
            )}
          >
            <div className="flex items-center gap-2">
              <span className="font-medium">⤷ tool_result</span>
              <span className="font-mono text-[10px] text-muted-foreground">{tr.tool_call_id}</span>
              <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(tr.content))}</span>
              <ToolResultBackLink
                state={state}
                originSeq={entry.origin?.call_sequence ?? null}
                originToolName={entry.origin?.tool_name ?? null}
                onJump={ctx.onJump}
                className="ml-auto"
              />
            </div>
            <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{tr.content}</pre>
          </div>
        )
      })}
      {parsed.extraUserText && (
        <div className="rounded border border-blue-200 bg-blue-50/60 p-3 text-[11px] dark:border-blue-900/40 dark:bg-blue-900/10">
          <Markdown text={parsed.extraUserText} />
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 6: Run typecheck**

Run: `just quality ts`
Expected: `openai-chat.tsx` now typechecks alongside `anthropic.tsx`. `openai-responses.tsx` still fails.

- [ ] **Step 7: Do NOT commit yet**

Proceed to Task 11.

---

## Task 11: Migrate OpenAI-responses renderer + add OpenAiResponsesInputBlocks

**Files:**
- Modify: `console/src/components/call-renderers/openai-responses.tsx`

- [ ] **Step 1: Add OutputCtx/InputCtx types + imports**

```tsx
import { ToolUsePointer, ToolResultBackLink } from "@/components/turn-detail/tool-pointer"
import { classifyToolUseState, classifyToolResultState, type ToolIndex, type TurnForClassification } from "@/lib/turn-index"

interface OutputCtx {
  toolIndex: ToolIndex
  callId: string
  isFinalCall: boolean
  turn: TurnForClassification
  onJump?: (sequence: number) => void
}
interface InputCtx {
  toolIndex: ToolIndex
  onJump?: (sequence: number) => void
}
```

- [ ] **Step 2: Thread ctx through ItemView for function_call items**

Locate the existing `ItemView` (renders a `ResponsesItem`). For items with `kind: "function_call"`, append a `ToolUsePointer`. Pattern similar to Task 10; the full surgery depends on the file's current structure but the behavior is: when rendering a function_call item, look up `ctx.toolIndex.get(item.call_id)`, classify, and append a pointer row.

Example sketch (adjust to match the file):

```tsx
function FunctionCallItemView({ item, ctx }: { item: ResponsesFunctionCall; ctx?: OutputCtx }) {
  const entry = ctx?.toolIndex.get(item.call_id) ?? { origin: null, resolution: null }
  const state = ctx ? classifyToolUseState(entry, { isFinalCall: ctx.isFinalCall, turn: ctx.turn }) : "healthy"
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">🔧 {item.name}</span>
        <span className="font-mono text-[10px] text-muted-foreground">{item.call_id}</span>
      </div>
      <details className="mt-1">
        <summary className="cursor-pointer text-muted-foreground text-[10px]">arguments</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(safeParseJson(item.arguments) ?? item.arguments)}
        </pre>
      </details>
      {ctx && (
        <div className="mt-1">
          <ToolUsePointer state={state} resolvedInSeq={entry.resolution?.call_sequence ?? null} onJump={ctx.onJump} />
        </div>
      )}
    </div>
  )
}
```

Update the main `ItemView` dispatch to route function_call items through this component when `ctx` is available.

- [ ] **Step 3: Update `OpenAiResponsesOutputBlocks` signature**

```tsx
export function OpenAiResponsesOutputBlocks({
  response,
  ctx,
  overlay,
}: {
  response: ResponsesResponse
  ctx: OutputCtx
  overlay?: CallOverlay | null
}) {
  if (response.output.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response items.</div>
  }
  return (
    <div className="space-y-2">
      <AggregatedOutputText text={response.output_text_aggregated} />
      {response.output.map((item, i) => <ItemView key={i} item={item} overlay={overlay} ctx={ctx} />)}
    </div>
  )
}
```

Add `ctx?: OutputCtx` prop to `ItemView` and thread it through to the function_call branch.

- [ ] **Step 4: Add `OpenAiResponsesInputBlocks` + `openaiResponsesParseForInput`**

```tsx
export interface OpenAiResponsesParsedInput {
  toolResults: Array<{ call_id: string; content: string }>
  extraUserText: string | null
}

export function openaiResponsesParseForInput(requestBody: string | null | undefined): OpenAiResponsesParsedInput {
  if (!requestBody) return { toolResults: [], extraUserText: null }
  const call = parseOpenAiResponsesCall(requestBody, null)
  // Walk the tail of input items — everything after the last function_call item.
  const items = call.request.input
  let lastCallIdx = -1
  for (let i = items.length - 1; i >= 0; i--) {
    if (items[i].kind === "function_call") { lastCallIdx = i; break }
  }
  const tail = items.slice(lastCallIdx + 1)
  const toolResults: OpenAiResponsesParsedInput["toolResults"] = []
  let extraUserText: string | null = null
  for (const item of tail) {
    if (item.kind === "function_call_output") {
      const content = typeof item.output === "string" ? item.output : formatJson(item.output)
      toolResults.push({ call_id: item.call_id, content })
    } else if (item.kind === "message" && item.role === "user") {
      const txt = typeof item.content === "string"
        ? item.content
        : item.content.map((p) => ("text" in p ? p.text : "")).join("")
      if (txt) extraUserText = txt
    }
  }
  return { toolResults, extraUserText }
}

export function OpenAiResponsesInputBlocks({
  parsed,
  ctx,
}: {
  parsed: OpenAiResponsesParsedInput
  ctx: InputCtx
  overlay?: CallOverlay | null
}) {
  if (parsed.toolResults.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.toolResults.map((tr) => {
        const entry = ctx.toolIndex.get(tr.call_id) ?? { origin: null, resolution: null }
        const state = classifyToolResultState(entry)
        return (
          <div
            key={tr.call_id}
            className={cn(
              "rounded border p-2 text-[11px]",
              state === "orphan"
                ? "bg-amber-50/60 border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40"
                : "bg-muted/40 border-border/60",
            )}
          >
            <div className="flex items-center gap-2">
              <span className="font-medium">⤷ tool_result</span>
              <span className="font-mono text-[10px] text-muted-foreground">{tr.call_id}</span>
              <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(tr.content))}</span>
              <ToolResultBackLink
                state={state}
                originSeq={entry.origin?.call_sequence ?? null}
                originToolName={entry.origin?.tool_name ?? null}
                onJump={ctx.onJump}
                className="ml-auto"
              />
            </div>
            <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{tr.content}</pre>
          </div>
        )
      })}
      {parsed.extraUserText && (
        <div className="rounded border border-blue-200 bg-blue-50/60 p-3 text-[11px] dark:border-blue-900/40 dark:bg-blue-900/10">
          <Markdown text={parsed.extraUserText} />
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 5: Full typecheck**

Run: `just quality ts`
Expected: all three renderers plus `dispatch.tsx` and `turn-index.ts` typecheck. `call-card.tsx` will still reference the old props — that's Task 12.

- [ ] **Step 6: Commit the renderer migration as a single logical unit**

(All three renderer files, `dispatch.tsx`, and `tool-pointer.tsx` must land together for the tree to compile.)

```bash
git add console/src/components/call-renderers/dispatch.tsx console/src/components/call-renderers/anthropic.tsx console/src/components/call-renderers/openai-chat.tsx console/src/components/call-renderers/openai-responses.tsx
git commit -m "refactor(console): migrate call renderers to ToolIndex with pointer states"
```

---

## Task 12: Rewrite CallCard with Input/Output subsections + Call#1/Call#last fusion

**Files:**
- Modify: `console/src/components/turn-detail/call-card.tsx`

- [ ] **Step 1: Replace `call-card.tsx` entirely**

```tsx
import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { Markdown } from "@/components/ui/markdown"
import { CallOutputDispatch, CallInputDispatch } from "@/components/call-renderers/dispatch"
import { CallChipDispatch } from "@/components/call-renderers/chips/dispatch"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"
import type { ToolIndex } from "@/lib/turn-index"

const SLOW_THRESHOLD_MS = 10_000

function classify(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

interface Props {
  call: AgentTurnCallItem
  turn: AgentTurnDetail
  toolIndex: ToolIndex
  isFirstCall: boolean
  active?: boolean
  defaultExpanded?: boolean
  onOpenDetail?: (id: string) => void
  onJumpToSequence?: (seq: number) => void
}

export function CallCard({
  call,
  turn,
  toolIndex,
  isFirstCall,
  active,
  defaultExpanded,
  onOpenDetail,
  onJumpToSequence,
}: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const speed = classify(call)
  const isFinalCall = call.id === turn.final_call_id
  const userInput = isFirstCall ? turn.user_input : null
  const finalAnswer = isFinalCall ? turn.final_answer : null

  return (
    <div
      id={`call-${call.sequence}`}
      className={cn(
        "rounded-lg border bg-background transition-colors",
        speed === "slow" && "border-l-2 border-l-amber-500/70 border-border",
        speed === "error" && "border-l-2 border-l-red-500/70 border-border",
        isFinalCall && speed === "normal" && "border-l-2 border-l-emerald-500/70 border-border",
        speed === "normal" && !isFinalCall && "border-border",
        active && "ring-2 ring-blue-400 ring-offset-1",
      )}
    >
      <button onClick={() => setExpanded((e) => !e)} className="w-full text-left">
        <div className="flex w-full items-center gap-3 px-3 py-2 text-left">
          <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
          {isFirstCall && (
            <span className="shrink-0 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
              👤 user
            </span>
          )}
          <CallChipDispatch
            wireApi={call.wire_api}
            callId={call.id}
            responseBody={call.response_body}
            finalCallId={turn.final_call_id}
          />
          <span className="flex-1 truncate text-xs text-muted-foreground">{call.model}</span>
          <span className={cn(
            "shrink-0 text-xs tabular-nums",
            speed === "slow" && "text-amber-600",
            speed === "error" && "text-red-600",
            speed === "normal" && "text-muted-foreground",
          )}>
            {speed === "error" && "✗ "}{formatMs(call.e2e_latency_ms)}
          </span>
          <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
            {formatNumber(call.input_tokens)}↑ {formatNumber(call.output_tokens)}↓
          </span>
          {expanded ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
        </div>
      </button>
      {expanded && (
        <div className="border-t border-border bg-muted/30 px-3 py-3 space-y-3">
          {/* Input subsection */}
          <section className="border-l-2 border-muted-foreground/30 pl-3">
            <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
              Input · request body
            </div>
            {userInput != null ? (
              <div className="rounded-lg border border-blue-200 bg-blue-50/60 p-3 dark:border-blue-900/40 dark:bg-blue-900/10">
                <Markdown text={userInput} />
              </div>
            ) : (
              <CallInputDispatch
                wireApi={call.wire_api}
                agentKind={turn.agent_kind ?? null}
                requestBody={call.request_body}
                toolIndex={toolIndex}
                onJump={onJumpToSequence}
              />
            )}
          </section>

          {/* Output subsection */}
          <section className="border-l-2 border-emerald-500/40 pl-3">
            <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
              Output · response body
            </div>
            {finalAnswer != null ? (
              <div className="rounded-lg border border-emerald-200 bg-emerald-50/60 p-3 dark:border-emerald-900/40 dark:bg-emerald-900/10">
                <div className="mb-1 text-xs font-medium text-emerald-800 dark:text-emerald-300">🎯 Final Answer</div>
                <Markdown text={finalAnswer} />
              </div>
            ) : null}
            <CallOutputDispatch
              wireApi={call.wire_api}
              agentKind={turn.agent_kind ?? null}
              responseBody={call.response_body}
              toolIndex={toolIndex}
              callId={call.id}
              finalCallId={turn.final_call_id}
              turn={{ final_call_id: turn.final_call_id, final_finish_reason: turn.final_finish_reason }}
              onJump={onJumpToSequence}
            />
          </section>

          <div className="text-[10px] text-muted-foreground font-mono">
            {call.model} · {call.wire_api} · TTFB {formatMs(call.ttft_ms)} · finish: {call.finish_reason ?? "—"}
          </div>
          <button onClick={() => onOpenDetail?.(call.id)} className="text-xs text-foreground hover:underline">
            View raw HTTP →
          </button>
        </div>
      )}
    </div>
  )
}
```

Notes:
- `isFirstCall` is true only for the call that matches `turn.user_call_id` (or, if that's null, for the first element in the calls array). The caller computes this — see Task 14.
- When `finalAnswer` is set, the final_answer block renders *above* `CallOutputDispatch`. The output dispatch still runs so that any trailing tool_use blocks (the "abnormal end" case the spec calls out) also appear, with their pointer degraded to `→ no response (turn ended)` by the classifier.
- Non-first, non-final calls skip both the user_input / final_answer blocks and render via the dispatcher only.

- [ ] **Step 2: Typecheck**

Run: `just quality ts`
Expected: `call-card.tsx` typechecks. `agent-turn-detail-panel.tsx` still fails because it doesn't yet pass the new props — that's Task 14.

- [ ] **Step 3: Do NOT commit yet**

Continue to Task 13.

---

## Task 13: Stats card — conditional Unresolved

**Files:**
- Modify: `console/src/components/turn-detail/stats-cards.tsx`

- [ ] **Step 1: Extend Props and compute unresolved count**

```tsx
import { AlertTriangle } from "lucide-react"
import { countUnresolved, type ToolIndex } from "@/lib/turn-index"
// ... existing imports

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  toolIndex: ToolIndex
  onJumpToSlowest?: (sequence: number) => void
  onJumpToFirstAnomaly?: () => void
}

export function StatsCards({ turn, calls, toolIndex, onJumpToSlowest, onJumpToFirstAnomaly }: Props) {
  // ... existing slowest + typeCounts memos

  const unresolved = useMemo(
    () => countUnresolved(toolIndex, { final_call_id: turn.final_call_id, final_finish_reason: turn.final_finish_reason }, turn.final_call_id),
    [toolIndex, turn.final_call_id, turn.final_finish_reason],
  )

  // ... first three cards unchanged ...

  // Replace the "Status" card with a conditional:
  return (
    <div className="grid grid-cols-4 gap-3">
      {/* ... Card 1, Card 2, Card 3 unchanged ... */}
      {unresolved > 0 ? (
        <button
          onClick={onJumpToFirstAnomaly}
          className="flex flex-col gap-0.5 rounded-lg border border-amber-200 bg-amber-50 px-3 py-2 text-left dark:border-amber-900/40 dark:bg-amber-900/10"
        >
          <span className="flex items-center gap-1 text-xs text-amber-700 dark:text-amber-400">
            <AlertTriangle className="size-3" /> Unresolved
          </span>
          <span className="text-sm font-medium tabular-nums text-amber-800 dark:text-amber-300">{unresolved}</span>
          <span className="text-[10px] text-amber-700 dark:text-amber-400">possible capture gap</span>
        </button>
      ) : (
        <Card label="Status">
          <div><TurnStatusBadge status={turn.status} /></div>
        </Card>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

Run: `just quality ts`
Expected: the StatsCards file typechecks. Panel still fails because it doesn't pass `toolIndex` yet.

- [ ] **Step 3: Do NOT commit yet**

Continue to Task 14.

---

## Task 14: Wire up `agent-turn-detail-panel.tsx`

**Files:**
- Modify: `console/src/pages/agent-turn-detail-panel.tsx`

- [ ] **Step 1: Replace `TurnDetailView` and the imports**

At the top of the file, drop `UserCard` and `FinalAnswerCard` from the barrel import, and add:

```tsx
import { useMemo } from "react"
import { buildToolIndex } from "@/lib/turn-index"
import { TopBar, StatsCards, GanttNav, CallCard } from "@/components/turn-detail"
```

Replace `TurnDetailView` with:

```tsx
function TurnDetailView({
  turn,
  calls,
  loadingCalls,
  activeSeq,
  onSelect,
  onOpenDetail,
}: {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  loadingCalls: boolean
  activeSeq: number | null
  onSelect: (seq: number) => void
  onOpenDetail: (id: string) => void
}) {
  const toolIndex = useMemo(() => buildToolIndex(calls), [calls])

  const userCallId = turn.user_call_id ?? calls[0]?.id ?? null
  const firstAnomalySeq = useMemo(() => {
    for (const c of calls) {
      // anomaly = this call's response contains a tool_use whose entry is capture_gap,
      // OR this call's request carries an orphan tool_result. Cheap heuristic: walk the
      // index once per call look-up is overkill; use the tool index entries directly.
      for (const [, entry] of toolIndex) {
        if (entry.origin?.call_id === c.id && entry.resolution == null && turn.final_call_id !== c.id) return c.sequence
        if (entry.origin == null && entry.resolution?.call_id === c.id) return c.sequence
      }
    }
    return null
  }, [calls, toolIndex, turn.final_call_id])

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 p-4 pb-0">
        <StatsCards
          turn={turn}
          calls={calls}
          toolIndex={toolIndex}
          onJumpToSlowest={onSelect}
          onJumpToFirstAnomaly={firstAnomalySeq != null ? () => onSelect(firstAnomalySeq) : undefined}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="flex flex-col gap-3">
          {loadingCalls && calls.length === 0 ? (
            <>
              {[0, 1, 2].map((i) => (
                <div key={i} className="h-12 animate-pulse rounded-lg border border-border bg-muted/40" />
              ))}
            </>
          ) : (
            calls.map((c) => (
              <CallCard
                key={c.id}
                call={c}
                turn={turn}
                toolIndex={toolIndex}
                isFirstCall={c.id === userCallId}
                active={c.sequence === activeSeq}
                defaultExpanded={c.sequence === activeSeq}
                onOpenDetail={onOpenDetail}
                onJumpToSequence={onSelect}
              />
            ))
          )}
          {!loadingCalls && calls.length === 0 && (
            <p className="text-center text-xs text-muted-foreground">No calls</p>
          )}
        </div>
      </div>
    </div>
  )
}
```

The standalone `UserCard` and `FinalAnswerCard` renders are gone. Their content now flows through `CallCard`'s Input/Output subsections.

- [ ] **Step 2: Typecheck and lint**

Run: `just quality ts`
Expected: all files typecheck and lint green.

- [ ] **Step 3: Run the full test suite**

Run: `cd console && bun test`
Expected: all green (existing wire-api tests + new turn-index tests).

- [ ] **Step 4: Commit the UI wiring**

```bash
git add console/src/components/turn-detail/call-card.tsx console/src/components/turn-detail/stats-cards.tsx console/src/pages/agent-turn-detail-panel.tsx
git commit -m "refactor(console): fuse user/final into call cards, add Unresolved stats card"
```

---

## Task 15: Delete dead components + update exports

**Files:**
- Delete: `console/src/components/turn-detail/user-card.tsx`
- Delete: `console/src/components/turn-detail/final-answer-card.tsx`
- Modify: `console/src/components/turn-detail/index.ts`

- [ ] **Step 1: Confirm no remaining references**

Run: `grep -rn "UserCard\|FinalAnswerCard" console/src`
Expected: matches only inside `user-card.tsx` / `final-answer-card.tsx` / `index.ts`. If anything else shows up, address it before deleting.

- [ ] **Step 2: Delete the files**

```bash
rm console/src/components/turn-detail/user-card.tsx
rm console/src/components/turn-detail/final-answer-card.tsx
```

- [ ] **Step 3: Remove their exports from the barrel**

Edit `console/src/components/turn-detail/index.ts`:

```ts
export { TopBar } from "./top-bar"
export { StatsCards } from "./stats-cards"
export { GanttNav } from "./gantt-nav"
export { CallCard } from "./call-card"
export { RawHttpDrawer } from "./raw-http-drawer"
export { MetadataPopover } from "./metadata-popover"
export { ToolUsePointer, ToolResultBackLink } from "./tool-pointer"
```

- [ ] **Step 4: Full typecheck + test**

Run: `just quality ts && cd console && bun test`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add -A console/src/components/turn-detail/
git commit -m "chore(console): remove UserCard and FinalAnswerCard (fused into CallCard)"
```

---

## Task 16: Manual verification in dev server

**Files:** none (manual UI verification)

- [ ] **Step 1: Start the dev server**

Run: `just dev console` (or `cd console && bun run dev`)

- [ ] **Step 2: Open a recent turn detail**

Navigate to the agent-turns list, open a multi-call turn (ideally one with at least one tool_use, one tool_result, one slow call). Verify:

- [ ] No standalone User Input card above the first call.
- [ ] No standalone Final Answer card below the last call.
- [ ] Call#1 expanded — Input subsection shows the user message in a blue-tinted card. Output subsection shows assistant text + tool_use with `→ result in #M ✓` pointer(s).
- [ ] Clicking `→ result in #M` scrolls the target call into view (blue ring flash).
- [ ] A middle call's Input subsection shows tool_result blocks with `← from #N · ToolName` back-links. Clicking them scrolls to the origin call.
- [ ] Call#last expanded — Output subsection leads with an emerald "🎯 Final Answer" card; any trailing tool_use blocks (rare) show `→ no response (turn ended)`.
- [ ] Stats card 4 says "Status" (normal) if no capture gaps; or "⚠ Unresolved N" if any.

- [ ] **Step 3: Force a capture-gap scenario**

If no naturally-occurring turn has gaps, skip this step. Otherwise verify the `⚠ result not captured` amber pointer renders on the tool_use side, and `⚠ origin not captured` on any orphan tool_result.

- [ ] **Step 4: No commit**

Manual step only. If UI issues surface, open new tasks to address them.

---

## Self-Review Notes

**Spec coverage checklist (post-plan):**

- [x] Call#1 fuses user_input into Input subsection → Task 12 (`userInput != null ? ... : CallInputDispatch`).
- [x] Call#last fuses final_answer into Output subsection → Task 12 (emerald block before CallOutputDispatch).
- [x] UserCard / FinalAnswerCard deleted → Task 15.
- [x] Input/Output subsections with grey/emerald borders → Task 12.
- [x] Turn-scoped ToolIndex replaces N+1 lookup → Tasks 1-5.
- [x] First-wins deduplication → Task 5 (third test).
- [x] Four pointer states → Tasks 6, 7 (component), 9/10/11 (wired into renderers).
- [x] A2′ strip rejection: not implemented — consistent with spec decision.
- [x] Stats card 4 conditional Unresolved → Task 13.
- [x] Click Unresolved → jump to first anomaly → Task 14 (`firstAnomalySeq` memo).
- [x] Jump does not force-open target call → Task 7 (`ToolUsePointer` onJump only emits sequence; existing `handleSelect` in the panel scrolls without expanding).
- [x] Raw HTTP drawer unchanged for cross-call linking → Task 9 step 4 (`drawerCtx` with empty index).
- [x] Unit tests for buildToolIndex and classifiers → Tasks 1-6.
- [ ] Component / visual snapshot tests → **deferred per spec's Open Questions** (would add a heavy test infra; manual verification in Task 16 suffices for MVP).

**Placeholder scan:** no TBD / TODO / "fill in details" in the plan body.

**Type consistency:** `ToolIndex`, `ToolIndexEntry`, `ToolUseState`, `ToolResultState`, `TurnForClassification`, `OutputCtx`, `InputCtx` names all used consistently across tasks. Renderer exports (`anthropicParseForInput`, `AnthropicInputBlocks`, etc.) use the three-wire-api mirror pattern consistently.
