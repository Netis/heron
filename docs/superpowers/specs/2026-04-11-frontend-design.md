# Frontend Design Spec

## Scope

Design the complete TokenScope frontend — a React SPA for monitoring and diagnosing LLM API performance. Covers page structure, navigation, layout per page, drilldown flow, and data sources. Implementation uses React + TypeScript, shadcn/ui + Tailwind CSS, Bun + Vite, React Router.

## Target Users

- **Ops**: Real-time health, error spikes, rate limiting, concurrency
- **Dev**: Per-request diagnosis, latency breakdown, body inspection, agent turn tracing
- **Business**: Token usage, model distribution, TPOT trends

## Global Layout

### Navigation — Collapsible Sidebar (VS Code style)

- **Collapsed (default):** 44px icon rail on the left edge, always visible
- **Expanded:** ~200px sidebar with icon + label, triggered by hamburger or hover
- **7 nav items:** Overview, Performance, Traffic, Errors, Models, Requests, Turns

### Global Toolbar — Fixed Top Bar

Shared across all pages, sticky at the top:

#### Time Range Selector

Single dropdown button with three sections:

```
┌─────────────────────────────────┐
│ Quick Select                     │
│ [5m] [15m] [1h] [6h] [24h] [7d] │
├─────────────────────────────────┤
│ Custom Range                     │
│ From: [datetime-local]           │
│ To:   [datetime-local]  [Apply]  │
├─────────────────────────────────┤
│ Auto Refresh                     │
│ [Off] [5s] [10s] [30s] [1m]     │
└─────────────────────────────────┘
```

- **Button label**: Preset → `Last 1h`; Custom → `MM-DD HH:mm ~ MM-DD HH:mm`; Auto-refresh active → append `↻ 10s` (↻ icon spins only during data fetch, static otherwise)
- **Quick Select**: Clicking a preset closes dropdown. Selected item uses inverted style.
- **Custom Range**: From/To datetime inputs + Apply. Validates start < end.
- **Auto Refresh**: Only available for preset (relative) ranges. Custom range automatically disables it. Greyed out with hint when custom. Selecting interval does NOT close dropdown. Refresh mechanism: re-triggers preset to slide the time window forward, all pages benefit via global state.
- **Granularity**: Auto-computed, not user-selectable. ≤15m→10s, ≤2h→1m, ≤24h→5m, >24h→1h.

#### Dimension Filters

Three multi-select dropdowns after the time range selector: **Provider**, **Model**, **Server IP**. Options fetched from `/api/filters/{providers,models,server_ips}`. Button shows count badge + clear button when active. Values passed as CSV to all API calls.

#### Page-Specific Filters

Pages may add page-level filters below the toolbar. Requests page adds **Status Code** (200/400/401/403/404/429/500/502/503) and **Finish Reason** (complete/stop/length/tool_use/error/cancelled) dropdowns.

## Pages

### Page 1: Overview 总览 (`/`)

Dashboard-level summary. First page users see.

**Top row — KPI cards (6):**

| Card | Value | Subtext |
|------|-------|---------|
| Total Requests | count | vs previous period % change |
| Avg TTFB | ms | with sparkline |
| Avg E2E Latency | ms | with sparkline |
| Error Rate | % | color-coded (green/yellow/red) |
| Total Tokens | count | in + out breakdown |
| Avg TPOT | ms/tok | streaming requests only |

**Middle row — 2 charts:**

- **Request Volume** (left): Stacked area chart by provider, X = time, Y = request count per bucket
- **Latency Overview** (right): Line chart with TTFB p50/p95 and E2E p50/p95

**Bottom row — 2 panels:**

- **Model Breakdown** (left): Horizontal bar chart — top N models by request count, showing tokens and avg latency inline
- **Error Rate by Model** (right): Horizontal bar chart — error rate % per model, color-coded segments for 4xx / 429 / 5xx

**Data source:** `llm_metrics`.

### Page 2: Performance 性能分析 (`/performance`)

Deep dive into latency and TPOT.

**Top row — 2 main charts:**

- **TTFB Distribution** (left): Line chart — p50 / p95 / p99 over time. Toggle by model via legend
- **E2E Latency Distribution** (right): Line chart — p50 / p95 / p99 over time

**Middle row — 2 charts:**

- **TPOT (Time Per Output Token)** (left): Line chart — ms/tok p50 / p95 over time (streaming requests only)
- **Concurrency** (right): Area chart — avg and max concurrent requests over time

**Bottom row — 2 charts:**

- **Cache Token Usage** (left): Stacked area chart — cache_read vs cache_creation input tokens over time. Shows cache effectiveness trend
- **Token Averages** (right): Line chart — input_tokens_avg and output_tokens_avg over time

**Data source:** `llm_metrics`.

### Page 3: Traffic 流量分析 (`/traffic`)

Volume, token usage, and model distribution.

**Top row — 2 charts:**

- **Request Volume** (left): Stacked bar chart by provider over time
- **Token Usage** (right): Stacked area chart — input tokens vs output tokens over time

**Middle row — 2 charts:**

- **Model Distribution** (left): Pie/donut chart — request count share per model
- **Finish Reason Breakdown** (right): Stacked bar chart — complete / length / tool_use / error / cancelled per time bucket

**Bottom row — 2 panels:**

- **Token Averages** (left): Line chart — input_tokens_avg and output_tokens_avg over time
- **Top Models Table** (right): Table — Model, Requests, Tokens (in/out), Avg TTFB, Avg E2E, Error %. Sortable columns. Clickable rows → Models page filtered

**Data source:** `llm_metrics`.

### Page 4: Errors 错误分析 (`/errors`)

Error monitoring and diagnosis.

**Top row — KPI cards (4):**

| Card | Value |
|------|-------|
| Total Errors | count + % of all requests |
| 4xx Count | with 429 rate-limit subset highlighted |
| 5xx Count | server error count |
| Error Rate Trend | sparkline |

**Middle row — 2 charts:**

- **Error Timeline** (left): Stacked area chart — 4xx vs 5xx vs 429 over time
- **Error by Model** (right): Grouped bar chart — error count per model, color-coded by error type

**Bottom row — 2 charts:**

- **Error Rate by Model** (left): Horizontal bar chart — error rate % per model, sorted by error rate desc. Segments for 4xx / 429 / 5xx
- **429 Rate Limiting Trend** (right): Line chart — 429 count over time, helps identify rate limiting patterns and bursts

**Data source:** `llm_metrics`.

### Page 5: Models 模型对比 (`/models`)

Per-model comparison and drilldown.

**Top — Model Comparison Table (full width):**

| Column | Description |
|--------|-------------|
| Model | Model name |
| Provider | Provider name |
| Requests | Total request count |
| Error % | Error rate |
| TTFB (avg/p95) | Latency breakdown |
| E2E (avg/p95) | End-to-end latency |
| TPOT (avg) | ms/token |
| Tokens (in/out) | Total token consumption |

Sortable by any column. Clicking a model row expands inline or navigates to a filtered detail view.

**Bottom — Selected Model Detail (2 charts):**

- **Latency Over Time** (left): Line chart — TTFB p50/p95 + E2E p50/p95 for selected model
- **Request Volume & Error Rate** (right): Dual-axis chart — bar = request volume, line = error rate %

**Data source:** `llm_metrics` (grouped by model dimension).

### Page 6: Requests 请求明细 (`/requests`)

Per-request detail — core diagnosis page.

**Initial state — Full-width table:**

| Column | Width | Description |
|--------|-------|-------------|
| Time | 70px | Request timestamp |
| Provider | 60px | Provider name |
| Model | 110px | Model name |
| Status | 40px | HTTP status code, color-coded |
| S | 26px | Stream indicator (⚡ or —) |
| Finish | 60px | Finish reason badge (color-coded) |
| TTFB | 55px | Time to first byte |
| E2E | 55px | End-to-end latency |
| In | 45px | Input token count |
| Out | 45px | Output token count |

Supports: search (model, path, response_id), column sorting, pagination. Global toolbar adds Status and Finish Reason filter dropdowns.

**Click a row → Slide-over overlay panel (~60% width):**

Panel slides in from right, covering the table (table dimmed behind). Panel contents:

1. **Header:** "Request Detail" + ↑↓ prev/next navigation + ✕ close button
2. **Summary cards (4):** Provider/Model, Status/Finish, TTFB/E2E, Tokens in/out
3. **Timeline:** Horizontal bar — TTFB phase (amber) + Generation phase (blue), with start/end timestamps
4. **Metadata:** ID, Response ID, Path, Client IP, Server IP, Stream, Throughput, Turn ID (clickable → Turns page)
5. **Request Headers:** Collapsible, with count badge
6. **Response Headers:** Collapsible, with count badge
7. **Request Body:** Collapsible, JSON formatted, scrollable
8. **Response Body:** Collapsible, JSON formatted, scrollable

**Interactions:**
- ↑/↓ buttons navigate to prev/next request without closing the panel
- Turn ID link navigates to Turns page filtered to that turn
- ✕ closes the panel, returns to table

**Data source:** `llm_calls`.

### Page 7: Turns 交互追踪 (`/turns`)

Agent turn tracking — groups multiple LlmCalls into a single agent interaction.

**Initial state — Full-width table:**

| Column | Description |
|--------|-------------|
| Time | Turn start time |
| Turn ID | Truncated turn identifier |
| Model | Primary model used |
| Status | Complete / Incomplete badge |
| Calls | Number of LlmCalls in this turn |
| Tokens | Total tokens (in + out) |
| Duration | Total turn duration |

**Click a Turn → Slide-over overlay panel (~92% width):**

Panel slides in from right, covering the table (table dimmed behind). Internal left/right split:

**Left panel (~260px) — LlmCall list:**
- **Header:** "Agent Turn" text (clickable, switches right panel back to Turn detail) + turn ID
- **Call cards:** Vertical list of LlmCall cards, each showing: sequence number, finish reason badge, timestamp, TTFB/E2E mini timeline bar, token counts

**Right panel (remaining space) — Detail view with ✕ close button at top-right:**

The right panel switches between two views:

*Default (Turn detail):*
1. **Summary cards:** Calls count, Total Tokens (in/out breakdown), Duration
2. **Status / Model cards**
3. **Metadata:** Turn ID, Start/End time, Client IP, Server IP
4. **Call Timeline:** Gantt-style chart showing all calls as horizontal bars on a shared time axis, with TTFB (amber) + generation (blue/green) phases

*After clicking a Call card (Call detail):*
1. **Summary cards (4):** Provider/Model, Status/Finish, TTFB/E2E, Tokens
2. **Timeline:** Single-call horizontal bar with timestamps
3. **Metadata:** Call ID, Response ID, Path, Stream
4. **Headers:** Request/Response headers, collapsible
5. **Body:** Request/Response body, JSON formatted, collapsible

**Switching back:** Click "Agent Turn" text in the left panel header → right panel returns to Turn detail.

**Close:** ✕ button in top-right corner of the right panel closes the entire overlay, returning to the Turn table.

**Data source:** `llm_turns` for table, `llm_calls` for call list and detail.

## Drilldown Navigation

| From | Action | To |
|------|--------|----|
| Traffic → Top Models | Click row | Models (filtered) |
| Models → Model row | Click row | Models detail (inline expand or filtered view) |
| Requests → Turn ID link | Click link | Turns (filtered to that turn) |
| Turns → Call card | Click card | In-panel detail switch (no page navigation) |

## Shared Interaction Patterns

1. **Slide-over overlay:** Used by both Requests and Turns pages. Panel slides from right, covers underlying table (dimmed), has ✕ close button. No nested routing — state managed by React component.
2. **Clickable table rows:** All data tables support row click for drilldown (either in-page overlay or cross-page navigation with filters).
3. **Responsive charts:** All time-series charts respect the global time range and granularity. Legend items toggle series visibility.
4. **Color coding:** Green = success/complete, Amber = warning/length, Red = error, Blue = tool_use/streaming, Purple = turns/agent.

## Tech Notes

- **Routing:** React Router with 7 routes (`/`, `/performance`, `/traffic`, `/errors`, `/models`, `/requests`, `/turns`)
- **State management:** URL query params for global filters (time range, provider, model) so views are shareable/bookmarkable
- **Data fetching:** REST API from `ts-api` crate. Charts poll on auto-refresh interval. Tables support pagination via API.
- **Charts:** Use a charting library compatible with React (e.g., Recharts, Tremor, or similar). Must support: line, area, bar, stacked, scatter, dual-axis.
- **Overlay state:** Component-level state (not URL routing). Opening a request/turn detail does not change the URL path.
