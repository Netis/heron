# README screenshot regenerator

Drives a running Heron console with Playwright and writes
PNGs into `docs/images/`. Used to refresh the README hero shots.

## Setup (once per machine)

```sh
cd scripts/screenshots
npm install
npx playwright install chromium
```

## Run

```sh
# Point at any Heron instance with real agent traffic.
# WINDOW_HOURS controls the time-range query string (default 24).
# TURN_ID deep-links the agent-turn-detail shot to a specific
# many-call run — pick one with ≥100 calls so the gantt is dense.
BASE=http://heron-host:4500 \
WINDOW_HOURS=24 \
TURN_ID=019e4242-9b82-7083-8cba-a046f3477e44 \
OUT=$PWD/../../docs/images \
node snap.mjs
```

Picks for `TURN_ID` come from:

```sh
curl -sS "$BASE/api/agent-turns?start=...&end=...&page_size=20&sort_by=call_count&sort_order=desc" \
  | jq '.data.items[] | {turn_id, agent_kind, call_count, primary_model}'
```

## What gets written

| File | Page |
|---|---|
| `overview.png` | `/` — agent activity + distribution + call-rate / latency / model panels |
| `agent-turns.png` | `/agent-turns` — list view sorted by call_count |
| `agent-turn-detail.png` | `/agent-turns?selected=<id>` — gantt timeline + per-call drilldown (skipped if `TURN_ID` not set) |
| `services-table.png` | `/services` — per-endpoint table |
| `services-path.png` | `/services` — Path tab, the service-to-service topology graph |
| `agent-session-detail.png` | `/agent-sessions` with first row clicked open |

Each shot is 1600×1000 viewport @2× device-scale, no full-page —
that keeps the README images digestible without scroll.
