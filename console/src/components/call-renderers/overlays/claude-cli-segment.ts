/**
 * Pure segmentation of Claude Code user-message text. Separated from the
 * React component so bun test can exercise it without resolving the UI
 * module graph. See claude-cli.tsx for the rendering side.
 */

export type Segment =
  | { kind: "plain"; text: string }
  | { kind: "system-reminder"; body: string }
  | { kind: "command"; name: string; message: string; args: string }
  | { kind: "local-command-stdout"; body: string }

const BLOCK_OPENERS = ["<system-reminder>", "<command-name>", "<local-command-stdout>"] as const

function findNextBlockStart(text: string, start: number): { idx: number; tag: string } | null {
  let best: { idx: number; tag: string } | null = null
  for (const tag of BLOCK_OPENERS) {
    const i = text.indexOf(tag, start)
    if (i === -1) continue
    if (best == null || i < best.idx) best = { idx: i, tag }
  }
  return best
}

function extractTagContent(
  text: string,
  start: number,
  openTag: string,
  closeTag: string,
): { content: string; endIndex: number } | null {
  const open = start + openTag.length
  const close = text.indexOf(closeTag, open)
  if (close === -1) return null
  return { content: text.slice(open, close), endIndex: close + closeTag.length }
}

/**
 * Segment a raw user-message string into plain text + scaffold blocks.
 * A `<command-name>` block optionally consumes adjacent `<command-message>`
 * and `<command-args>` tags. Unclosed tags spill their remainder as plain
 * text (safe-fail — never silently drop content).
 */
export function segmentClaudeCliUserText(input: string): Segment[] {
  const segs: Segment[] = []
  let i = 0
  while (i < input.length) {
    const next = findNextBlockStart(input, i)
    if (!next) {
      const tail = input.slice(i)
      if (tail.length > 0) segs.push({ kind: "plain", text: tail })
      break
    }
    if (next.idx > i) {
      segs.push({ kind: "plain", text: input.slice(i, next.idx) })
    }
    if (next.tag === "<system-reminder>") {
      const x = extractTagContent(input, next.idx, "<system-reminder>", "</system-reminder>")
      if (!x) {
        segs.push({ kind: "plain", text: input.slice(next.idx) })
        break
      }
      segs.push({ kind: "system-reminder", body: x.content })
      i = x.endIndex
    } else if (next.tag === "<local-command-stdout>") {
      const x = extractTagContent(input, next.idx, "<local-command-stdout>", "</local-command-stdout>")
      if (!x) {
        segs.push({ kind: "plain", text: input.slice(next.idx) })
        break
      }
      segs.push({ kind: "local-command-stdout", body: x.content })
      i = x.endIndex
    } else if (next.tag === "<command-name>") {
      const nm = extractTagContent(input, next.idx, "<command-name>", "</command-name>")
      if (!nm) {
        segs.push({ kind: "plain", text: input.slice(next.idx) })
        break
      }
      let cur = nm.endIndex
      let message = ""
      let args = ""
      const msgOpen = "<command-message>"
      if (input.startsWith(msgOpen, cur)) {
        const mx = extractTagContent(input, cur, msgOpen, "</command-message>")
        if (mx) {
          message = mx.content
          cur = mx.endIndex
        }
      }
      const argsOpen = "<command-args>"
      if (input.startsWith(argsOpen, cur)) {
        const ax = extractTagContent(input, cur, argsOpen, "</command-args>")
        if (ax) {
          args = ax.content
          cur = ax.endIndex
        }
      }
      segs.push({ kind: "command", name: nm.content, message, args })
      i = cur
    } else {
      break
    }
  }
  return segs
}
