/**
 * Heron brand mark. A filled silhouette of a heron in profile — long
 * beak, S-curved neck, body, and two trailing legs, with a knocked-out
 * eye. The icon-only and wordmark variants share the exact same glyph so
 * they line up visually when the sidebar collapses.
 *
 * Fill + stroke use `currentColor` so the mark inherits the surrounding
 * text colour and respects light/dark themes. The eye is a true cut-out
 * (evenodd sub-path) rather than a painted dot, so it shows whatever is
 * behind the mark instead of a hard-coded background colour.
 */

import { cn } from "@/lib/utils"

interface LogoProps {
  variant: "icon" | "wordmark"
  className?: string
}

// Body + S-neck + head + beak, with the eye as an evenodd cut-out.
const HERON_BODY =
  "M4.8 17.2 C6.2 13.2 9.8 12.6 11.6 13.2 C11.9 11 12.8 9 14.2 7.9 " +
  "C15.3 7 16.4 6.6 17.2 6.3 L22 5.0 L17.8 7.2 C16.9 7.7 16.2 8.7 15.6 9.9 " +
  "C14.6 12 13.9 14.2 13.2 15.4 C12.3 17.2 10 18.8 7.6 18.4 " +
  "C6.4 18.2 5.4 17.8 4.8 17.2 Z " +
  "M16.18 7.4 A0.42 0.42 0 1 0 17.02 7.4 A0.42 0.42 0 1 0 16.18 7.4 Z"

// Two legs.
const HERON_LEGS = "M9.2 18.2 L8.3 22 M11 17.8 L12 22"

function HeronGlyph() {
  return (
    <>
      <path d={HERON_BODY} fill="currentColor" fillRule="evenodd" stroke="none" />
      <path
        d={HERON_LEGS}
        fill="none"
        stroke="currentColor"
        strokeWidth={1}
        strokeLinecap="round"
      />
    </>
  )
}

export function Logo({ variant, className }: LogoProps) {
  if (variant === "icon") {
    return (
      <svg
        viewBox="0 0 24 24"
        className={cn("shrink-0", className)}
        aria-label="Heron"
        role="img"
      >
        <HeronGlyph />
      </svg>
    )
  }

  // Wordmark: heron glyph on the left + "Heron" set in a system-stack
  // semi-bold. SVG <text> renders crisply at any DPI and tints with
  // currentColor; we accept the (tiny) variance across OS font choices
  // in exchange for not shipping a webfont.
  return (
    <svg
      viewBox="0 0 156 24"
      fill="none"
      className={cn("shrink-0", className)}
      aria-label="Heron"
      role="img"
    >
      <HeronGlyph />
      <text
        x={30}
        y={17}
        fontFamily='ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif'
        fontWeight={600}
        fontSize={15}
        letterSpacing={-0.2}
        fill="currentColor"
      >
        Heron
      </text>
    </svg>
  )
}
