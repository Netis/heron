import { AnthropicCallView, AnthropicOutputBlocks, AnthropicInputBlocks, anthropicParseForOutput, anthropicParseForInput } from "./anthropic"
import { GeminiAiStudioCallView, GeminiAiStudioOutputBlocks, GeminiAiStudioInputBlocks, geminiAiStudioParseForOutput, geminiAiStudioParseForInput } from "./gemini-aistudio"
import { OpenAiChatCallView, OpenAiChatOutputBlocks, OpenAiChatInputBlocks, openaiChatParseForOutput, openaiChatParseForInput } from "./openai-chat"
import { OpenAiResponsesCallView, OpenAiResponsesOutputBlocks, OpenAiResponsesInputBlocks, openaiResponsesParseForOutput, openaiResponsesParseForInput } from "./openai-responses"
import { RawJsonFallback } from "./fallback"
import { ClaudeCliOverlay } from "./overlays/claude-cli"
import type { CallOverlay } from "./overlays/types"
import type { ToolIndex } from "@/lib/turn-index"

const agentOverlays: Record<string, CallOverlay> = {
  "claude-cli": ClaudeCliOverlay,
  // codex-cli overlay deferred until business feedback surfaces a concrete need.
}

function overlayFor(agentKind: string | null): CallOverlay | null {
  if (!agentKind) return null
  return agentOverlays[agentKind] ?? null
}

// ── full detail view (raw HTTP drawer) ────────────────────────────────────

export interface CallRendererDispatchProps {
  wireApi: string
  agentKind?: string | null
  requestBody: string | null
  responseBody: string | null
  hasRequestBody: boolean
}

/**
 * Top-level renderer for an LLM Call detail panel. Dispatches by wire_api to
 * a provider-specific view. Unknown wire_api falls through to a raw JSON
 * fallback. The drawer view is single-call scoped and does not receive a
 * turn-wide ToolIndex — tool pointers inside degrade to "⚠ result not captured"
 * which is acceptable since cross-call resolution is not meaningful here.
 */
export function CallRendererDispatch(props: CallRendererDispatchProps) {
  const overlay = overlayFor(props.agentKind ?? null)
  switch (props.wireApi) {
    case "anthropic":
      return (
        <AnthropicCallView
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          overlay={overlay}
          hasRequestBody={props.hasRequestBody}
        />
      )
    case "openai-chat":
      return (
        <OpenAiChatCallView
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          overlay={overlay}
          hasRequestBody={props.hasRequestBody}
        />
      )
    case "openai-responses":
      return (
        <OpenAiResponsesCallView
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          overlay={overlay}
          hasRequestBody={props.hasRequestBody}
        />
      )
    case "gemini-aistudio":
      return (
        <GeminiAiStudioCallView
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          hasRequestBody={props.hasRequestBody}
        />
      )
    default:
      return (
        <RawJsonFallback
          wireApi={props.wireApi}
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          hasRequestBody={props.hasRequestBody}
        />
      )
  }
}

// ── Output subsection (CallCard expanded) ─────────────────────────────────

export interface CallOutputDispatchProps {
  wireApi: string
  agentKind: string | null
  responseBody: string | null
  toolIndex: ToolIndex
  callId: string
}

/**
 * Output-only dispatcher used by the turn-detail CallCard expanded state.
 * Renders only the Output section (assistant text, reasoning, tool_use blocks
 * with ToolUsePointer). Takes a turn-scoped ToolIndex so pointers can inline-
 * echo results across calls.
 */
export function CallOutputDispatch(props: CallOutputDispatchProps) {
  const overlay = overlayFor(props.agentKind)
  const ctx = {
    toolIndex: props.toolIndex,
    callId: props.callId,
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
    case "gemini-aistudio": {
      const { response } = geminiAiStudioParseForOutput(null, props.responseBody)
      return <GeminiAiStudioOutputBlocks response={response} />
    }
    default:
      return (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-[11px] text-muted-foreground">
          No output renderer for wire_api "{props.wireApi}". Open raw HTTP for details.
        </div>
      )
  }
}

// ── Input subsection (CallCard expanded, non-first calls) ─────────────────

export interface CallInputDispatchProps {
  wireApi: string
  agentKind: string | null
  requestBody: string | null
  toolIndex: ToolIndex
}

/**
 * Input-only dispatcher for non-first calls. Renders the "delta" of a request
 * body — primarily the tool_result blocks returning from the previous call's
 * tool_use, plus any new user text (rare multi-turn case).
 */
export function CallInputDispatch(props: CallInputDispatchProps) {
  const overlay = overlayFor(props.agentKind)
  const ctx = { toolIndex: props.toolIndex }
  switch (props.wireApi) {
    case "anthropic":
      return <AnthropicInputBlocks parsed={anthropicParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    case "openai-chat":
      return <OpenAiChatInputBlocks parsed={openaiChatParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    case "openai-responses":
      return <OpenAiResponsesInputBlocks parsed={openaiResponsesParseForInput(props.requestBody)} overlay={overlay} ctx={ctx} />
    case "gemini-aistudio":
      return <GeminiAiStudioInputBlocks parsed={geminiAiStudioParseForInput(props.requestBody)} />
    default:
      return null
  }
}
