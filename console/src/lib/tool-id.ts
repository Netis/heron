/**
 * Tool-id canonicalization (frontend mirror of `ts-llm::agents::generic_common::canonicalize_tool_id`).
 *
 * Restores the LLM-side `prefix_<rest>` form when a client has stripped the
 * underscore between the prefix and the id body. Observed in OpenClaw
 * (OpenAI/JS SDK + GLM model) — emits `call_d9c1...` over the wire but echoes
 * `calld9c1...` when reflecting `assistant.tool_calls[]` into subsequent
 * request bodies. Without this, tool_use (canonical) and tool_result
 * (stripped) appear as different ids in the ToolIndex.
 *
 * Both sides of the index must canonicalize using the same rule so the
 * lookup hits regardless of whether the id was sourced from the response
 * stream or the echoed messages history.
 *
 * Future client quirks (lowercase, prefix swap) are added here as small
 * targeted patches; this is intentionally whack-a-mole, mirroring the Rust
 * impl exactly.
 */
const PREFIXES = ["call", "toolu", "fc", "chatcmpl"] as const

export function canonicalizeToolId(id: string): string {
  for (const p of PREFIXES) {
    if (!id.startsWith(p)) continue
    const after = id.slice(p.length)
    if (after.length > 0 && !after.startsWith("_")) {
      return `${p}_${after}`
    }
  }
  return id
}
