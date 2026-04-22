/**
 * Call classification for list-level aggregation (stats counts, chip color).
 *
 * This is the ONLY shared vocabulary between wire_apis — a 3-element enum
 * used purely for "count-by-kind" in stats cards. It is NOT a data abstraction
 * over provider-specific parse output.
 *
 * Each wire_api module exports its own `classifyType()` function that decides
 * which bucket a call falls into; the dispatch in ./dispatch.ts picks the
 * right one by wire_api string.
 */
export type CallType = "tool_call" | "text" | "final"
