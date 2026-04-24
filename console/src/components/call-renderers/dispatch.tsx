import { AnthropicCallView, AnthropicOutputBlocks, anthropicParseForOutput } from "./anthropic"
import { OpenAiChatCallView, OpenAiChatOutputBlocks, openaiChatParseForOutput } from "./openai-chat"
import { OpenAiResponsesCallView, OpenAiResponsesOutputBlocks, openaiResponsesParseForOutput } from "./openai-responses"
import { RawJsonFallback } from "./fallback"
import { ClaudeCliOverlay } from "./overlays/claude-cli"
import type { CallOverlay } from "./overlays/types"

const agentOverlays: Record<string, CallOverlay> = {
  "claude-cli": ClaudeCliOverlay,
  // codex-cli overlay deferred until business feedback surfaces a concrete need.
}

function overlayFor(agentKind: string | null): CallOverlay | null {
  if (!agentKind) return null
  return agentOverlays[agentKind] ?? null
}

export interface CallRendererDispatchProps {
  wireApi: string
  /** Optional — only passed when the call is rendered inside a turn (agent overlays apply there). */
  agentKind?: string | null
  requestBody: string | null
  responseBody: string | null
  /** Optional — supplied by turn-detail CallCard so tool_use blocks can show joined results. */
  nextCallRequestBody?: string | null
  hasRequestBody: boolean
}

/**
 * Top-level renderer for an LLM Call detail panel. Dispatches by wire_api to
 * a provider-specific view. Unknown wire_api falls through to a raw JSON
 * fallback. An agent_kind overlay (from ./overlays/*) may be passed to the
 * base renderer via slots.
 */
export function CallRendererDispatch(props: CallRendererDispatchProps) {
  const overlay = overlayFor(props.agentKind ?? null)
  switch (props.wireApi) {
    case "anthropic":
      return (
        <AnthropicCallView
          requestBody={props.requestBody}
          responseBody={props.responseBody}
          nextCallRequestBody={props.nextCallRequestBody}
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

// ── output-only variant ────────────────────────────────────────────────────

export interface CallOutputDispatchProps {
  wireApi: string
  agentKind: string | null
  requestBody: string | null
  responseBody: string | null
  nextCallRequestBody?: string | null
}

/**
 * Output-only dispatcher used by the turn-detail CallCard expanded state.
 * Renders only the Output section of the call (no Input, no Usage card —
 * CallCard shows call-level metadata separately).
 */
export function CallOutputDispatch(props: CallOutputDispatchProps) {
  const overlay = overlayFor(props.agentKind)
  switch (props.wireApi) {
    case "anthropic": {
      const { response, resultLookup } = anthropicParseForOutput(
        props.requestBody,
        props.responseBody,
        props.nextCallRequestBody,
      )
      return <AnthropicOutputBlocks response={response} resultLookup={resultLookup} overlay={overlay} />
    }
    case "openai-chat": {
      const { response } = openaiChatParseForOutput(props.requestBody, props.responseBody)
      return <OpenAiChatOutputBlocks response={response} />
    }
    case "openai-responses": {
      const { response } = openaiResponsesParseForOutput(props.requestBody, props.responseBody)
      return <OpenAiResponsesOutputBlocks response={response} overlay={overlay} />
    }
    default:
      return (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-[11px] text-muted-foreground">
          No output renderer for wire_api "{props.wireApi}". Open raw HTTP for details.
        </div>
      )
  }
}
