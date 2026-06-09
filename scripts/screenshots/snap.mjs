#!/usr/bin/env node
// Regenerate the README screenshots under docs/images/ by driving
// a real Heron console with Playwright. Point BASE at any
// running instance that has live agent traffic.
//
// Setup:
//   cd scripts/screenshots && npm install && npx playwright install chromium
// Run:
//   BASE=http://heron-host:4500 OUT=$PWD/../../docs/images node snap.mjs
//
// Why per-instance: the most striking screenshots come from instances
// with real fleet density — multi-leg proxy hops, dozens of services,
// agent turns with 100+ calls. Replays of pcap fixtures don't show
// the topology graph you'd put on a landing page.

import { chromium } from "playwright"
import { mkdirSync } from "node:fs"
import { resolve } from "node:path"

const BASE = process.env.BASE || "http://localhost:3000"
const OUT = process.env.OUT || resolve(process.cwd(), "../../docs/images")
const WINDOW_HOURS = Number(process.env.WINDOW_HOURS || 24)
const NOW = Math.floor(Date.now() / 1000)
const START = NOW - WINDOW_HOURS * 3600

// Optional: a specific turn id to deep-link the detail screenshot.
// Picking a turn with 100+ calls makes the Timeline gantt actually
// show off the agent reconstruction.
const TURN_ID = process.env.TURN_ID || ""

// Which console theme to capture in. The console persists the choice in
// localStorage under "heron-theme" (zustand persist), so we seed it before
// any page script runs rather than relying on the deployed default. "kami"
// (washi-paper light theme) is the README default; override with THEME=dark
// or THEME=light.
const THEME = process.env.THEME || "kami"

// Optional: deep-link the session-detail shot to a specific session, given as
// "source_id/session_id" (mirrors TURN_ID for turns). Pick a session with many
// turns so the transcript is rich. Unset → fall back to opening the first row
// of the sessions list.
const SESSION = process.env.SESSION || ""

const SHOTS = [
  {
    name: "overview.png",
    path: `/?start=${START}&end=${NOW}&preset=custom`,
    waitFor: "text=Agent Activity",
  },
  {
    name: "agent-turns.png",
    path: `/agent-turns?start=${START}&end=${NOW}&preset=custom&sort=call_count&order=desc`,
    waitFor: "table tbody tr",
  },
  {
    name: "agent-turn-detail.png",
    path: TURN_ID
      ? `/agent-turns?start=${START}&end=${NOW}&preset=custom&selected=${TURN_ID}`
      : null,
    waitFor: "text=Timeline",
    skip: !TURN_ID,
  },
  {
    name: "services-table.png",
    path: `/services?start=${START}&end=${NOW}&preset=custom`,
    waitFor: "table tbody tr",
  },
  {
    name: "services-path.png",
    path: `/services?start=${START}&end=${NOW}&preset=custom`,
    afterLoad: async (page) => {
      await page.getByRole("button", { name: /^Path$/ }).click()
      await page.waitForTimeout(800)
    },
  },
  {
    name: "traffic.png",
    path: `/traffic?start=${START}&end=${NOW}&preset=custom`,
    waitFor: "text=Input Tokens",
    afterLoad: async (page) => {
      // let the recharts series animate in before snapping
      await page.waitForTimeout(1000)
    },
  },
  {
    // The sessions list renders rows as <Link> anchors, not a <table>, so
    // target the row href. Deep-link via SESSION when provided (richer
    // transcript), else open the first row.
    name: "agent-session-detail.png",
    path: SESSION
      ? `/agent-sessions/${SESSION}?start=${START}&end=${NOW}&preset=custom`
      : `/agent-sessions?start=${START}&end=${NOW}&preset=custom`,
    waitFor: SESSION ? null : `a[href*="/agent-sessions/"]`,
    afterLoad: async (page) => {
      if (!SESSION) {
        await page.locator('a[href*="/agent-sessions/"]').first().click()
      }
      await page.waitForTimeout(1500)
    },
  },
  {
    name: "pipeline-health.png",
    path: `/debug/pipeline-health`,
    waitFor: "text=Pipeline Health",
    afterLoad: async (page) => {
      await page.waitForTimeout(800)
    },
  },
]

mkdirSync(OUT, { recursive: true })
const browser = await chromium.launch()
const ctx = await browser.newContext({
  viewport: { width: 1600, height: 1000 },
  deviceScaleFactor: 2,
})
// Seed the persisted theme before the app boots so every shot renders in the
// chosen theme. Must match the zustand-persist envelope used by the store
// (stores/theme.ts, key "heron-theme").
await ctx.addInitScript((theme) => {
  try {
    localStorage.setItem("heron-theme", JSON.stringify({ state: { theme }, version: 0 }))
  } catch {
    /* localStorage unavailable — fall back to the deployed default */
  }
}, THEME)
const page = await ctx.newPage()
console.log(`theme: ${THEME}`)

for (const shot of SHOTS) {
  if (shot.skip) {
    console.log(`-- ${shot.name}  skipped (no TURN_ID)`)
    continue
  }
  const url = BASE + shot.path
  console.log(`-> ${shot.name}  ${url}`)
  await page.goto(url, { waitUntil: "networkidle", timeout: 30_000 })
  if (shot.waitFor) {
    try {
      await page.waitForSelector(shot.waitFor, { timeout: 10_000 })
    } catch {
      console.warn(`   warn: waitFor "${shot.waitFor}" timed out — taking shot anyway`)
    }
  }
  if (shot.afterLoad) {
    try {
      await shot.afterLoad(page)
    } catch (e) {
      console.warn(`   warn: afterLoad: ${e.message}`)
    }
  }
  await page.waitForTimeout(600)
  const target = resolve(OUT, shot.name)
  await page.screenshot({ path: target, fullPage: false })
  console.log(`   ok ${target}`)
}

await browser.close()
console.log("done")
