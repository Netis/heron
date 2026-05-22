/**
 * TokenScope brand mark. Two variants share the same icon glyph (rounded
 * square "scope frame" containing three decreasing horizontal lines —
 * abstracted tokens viewed through the lens), so the icon-only and the
 * wordmark line up visually when the sidebar collapses.
 *
 * Stroke + fills use `currentColor` so the mark inherits the surrounding
 * text colour and respects light/dark themes without extra CSS.
 */

import { cn } from "@/lib/utils"

interface LogoProps {
  variant: "icon" | "wordmark"
  className?: string
}

export function Logo({ variant, className }: LogoProps) {
  if (variant === "icon") {
    return (
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth={1.75}
        strokeLinecap="round"
        strokeLinejoin="round"
        className={cn("shrink-0", className)}
        aria-label="TokenScope"
        role="img"
      >
        <rect x={2.5} y={2.5} width={19} height={19} rx={4.5} />
        <line x1={6.5} y1={9} x2={17.5} y2={9} />
        <line x1={6.5} y1={13} x2={14} y2={13} />
        <line x1={6.5} y1={17} x2={10.5} y2={17} />
      </svg>
    )
  }

  // Wordmark: same icon glyph at the left + "TokenScope" set in a
  // system-stack semi-bold. SVG <text> renders crisply at any DPI and
  // tints with currentColor; we accept the (tiny) variance across OS
  // font choices in exchange for not shipping a webfont.
  return (
    <svg
      viewBox="0 0 156 24"
      fill="none"
      className={cn("shrink-0", className)}
      aria-label="TokenScope"
      role="img"
    >
      <g stroke="currentColor" strokeWidth={1.75} strokeLinecap="round" strokeLinejoin="round">
        <rect x={2.5} y={2.5} width={19} height={19} rx={4.5} />
        <line x1={6.5} y1={9} x2={17.5} y2={9} />
        <line x1={6.5} y1={13} x2={14} y2={13} />
        <line x1={6.5} y1={17} x2={10.5} y2={17} />
      </g>
      <text
        x={30}
        y={17}
        fontFamily='ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif'
        fontWeight={600}
        fontSize={15}
        letterSpacing={-0.2}
        fill="currentColor"
      >
        TokenScope
      </text>
    </svg>
  )
}
