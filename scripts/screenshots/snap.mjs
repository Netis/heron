#!/usr/bin/env node
// Regenerate the README screenshots under docs/images/ by driving
// a real TokenScope console with Playwright. Point BASE at any
// running instance that has live agent traffic.
//
// Setup:
//   cd scripts/screenshots && npm install && npx playwright install chromium
// Run:
//   BASE=http://172.16.103.81:4500 OUT=$PWD/../../docs/images node snap.mjs
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
    name: "agent-session-detail.png",
    path: `/agent-sessions?start=${START}&end=${NOW}&preset=custom`,
    waitFor: "table tbody tr",
    afterLoad: async (page) => {
      await page.locator("table tbody tr").first().click()
      await page.waitForTimeout(1500)
    },
  },
]

mkdirSync(OUT, { recursive: true })
const browser = await chromium.launch()
const ctx = await browser.newContext({
  viewport: { width: 1600, height: 1000 },
  deviceScaleFactor: 2,
})
const page = await ctx.newPage()

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
