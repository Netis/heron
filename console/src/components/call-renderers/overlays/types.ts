/**
 * Agent-kind overlay — a small set of slot components that a base renderer
 * opts into when rendering user-visible text. Overlays do not parse bodies;
 * they post-process display strings (fold / highlight / filter agent-scaffold
 * artifacts that are not useful for the observer).
 *
 * Overlays are layered on top of per-wire_api renderers via the top-level
 * dispatch. A given agent_kind may or may not have an overlay.
 */
export interface CallOverlay {
  /** Transforms user-message text (e.g. fold `<system-reminder>` blocks). */
  UserMessageContent?: React.FC<{ text: string }>
  /** Transforms tool_result content (rarely needed — leave undefined to use default). */
  ToolResultContent?: React.FC<{ content: string; isError: boolean }>
}
