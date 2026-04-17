# Page 6: Requests 请求明细 — Implementation Spec

## Scope

Implement the Requests page (`/requests`) with a minimal app shell (collapsible sidebar, toolbar, routing). This is the first page built, so it establishes the foundational layout and data-fetching patterns.

## API Endpoints

### `GET /api/calls` — List

Query params: `start`, `end` (epoch seconds, required), `provider`, `model`, `server_ip`, `status_code`, `finish_reason` (comma-separated, optional), `sort_by` (default `request_time`), `sort_order` (default `desc`), `page` (default 1), `page_size` (default 50, max 200).

Response: `{ code: 0, message: "ok", data: { total: number, items: CallListItem[] } }`

```typescript
interface CallListItem {
  id: string
  request_time: number        // epoch seconds
  provider: string
  model: string
  status_code: number | null
  is_stream: boolean
  finish_reason: string | null // complete | length | tool_use | error | cancelled
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
}
```

### `GET /api/calls/{id}` — Detail

Response: `{ code: 0, message: "ok", data: CallDetail }`

```typescript
interface CallDetail {
  id: string
  request_time: number
  response_time: number | null
  complete_time: number | null
  provider: string
  model: string
  api_type: string
  is_stream: boolean
  request_path: string
  status_code: number | null
  finish_reason: string | null
  input_tokens: number | null
  output_tokens: number | null
  total_tokens: number | null
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  response_id: string | null
  tenant_id: string | null
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  request_body: string | null   // JSON string
  response_body: string | null  // JSON string
  request_headers: string | null // JSON string: [key, value][]
  response_headers: string | null // JSON string: [key, value][]
}
```

### Error response

`{ code: number, message: string, data: {} }` — codes: 1001 (bad param), 2001 (not found), 5001 (internal).

## App Shell (Minimal Foundation)

### Layout

```
┌─────┬──────────────────────────────────┐
│     │         Toolbar (h-12)           │
│ 44px├──────────────────────────────────┤
│ rail│                                  │
│     │         <Outlet />               │
│     │                                  │
└─────┴──────────────────────────────────┘
```

### Sidebar

- **Collapsed (default):** 44px icon rail, Lucide icons, tooltip on hover showing label
- **Expanded:** 200px, icon + label, triggered by hamburger icon at top
- **7 nav items:** Overview `/`, Performance `/performance`, Traffic `/traffic`, Errors `/errors`, Models `/models`, Requests `/requests`, Turns `/turns`
- Active route: highlighted background + accent color
- State: Zustand store (`useSidebarStore`)

### Toolbar

- Fixed top bar within the content area (right of sidebar)
- **Time range selector:** dropdown with presets — Last 5m / 15m / 1h / 6h / 24h / 7d
- Computes `start` (epoch seconds) and `end` (now, epoch seconds) from selection
- State: Zustand store (`useToolbarStore`) — `timeRange` preset string, computed `start`/`end` getters
- Granularity, dimension filters, auto-refresh: deferred

### Routing

React Router v7, all 7 routes under `AppLayout`. Only `/requests` has a real page component; the rest render a placeholder.

## Requests Page

### Table

Full-width table with these columns:

| Column | Width | Content |
|--------|-------|---------|
| Time | 160px | Formatted datetime (HH:mm:ss.SSS) |
| Provider | 80px | Provider name |
| Model | 140px | Model name, truncated with tooltip |
| Status | 52px | HTTP status badge, color-coded |
| S | 32px | ⚡ if streaming, — if not |
| Finish | 72px | Finish reason badge, color-coded |
| TTFB | 72px | Milliseconds, 1 decimal |
| E2E | 72px | Milliseconds, 1 decimal |
| In | 56px | Input token count |
| Out | 56px | Output token count |

**Color coding:**
- Status: 2xx → green, 4xx → amber, 429 → red, 5xx → red
- Finish: complete → green, length → amber, tool_use → blue, error → red, cancelled → gray

**Behaviors:**
- Row hover: highlight background, cursor-pointer
- Row click: opens slide-over detail panel
- Column header click: toggles sort (asc/desc) on that column, sends `sort_by`/`sort_order` to API
- Pagination: bottom of table — page info ("1-50 of 1234"), page size selector (20/50/100), prev/next buttons

**Data fetching:**
- `useRequests` hook using TanStack Query
- Query key: `['calls', { start, end, page, pageSize, sortBy, sortOrder }]`
- Reads `start`/`end` from toolbar store
- Pagination and sort state: component-local (`useState`)

### Slide-over Detail Panel

Slides in from right, 60% viewport width. Overlay dims the table behind (bg-black/20).

**Layout (top to bottom):**

1. **Header bar:** "Request Detail" title, ← → prev/next buttons (navigate within current page's items), ✕ close button (right-aligned)

2. **Summary cards (4, horizontal row):**
   - Provider / Model
   - Status code + Finish reason
   - TTFB / E2E latency
   - Tokens: input → output (total)

3. **Timeline bar:** Horizontal bar showing request lifecycle:
   - Full width = `complete_time - request_time` (E2E)
   - Amber segment = `response_time - request_time` (TTFB)
   - Blue segment = `complete_time - response_time` (Generation)
   - Timestamps labeled at start/end

4. **Metadata grid (2 columns):**
   - ID, Response ID, Path, Client IP:Port, Server IP:Port, Stream (yes/no), API Type, Tenant ID

5. **Collapsible sections (4):**
   - Request Headers (with count badge) — renders as key-value table
   - Response Headers (with count badge) — renders as key-value table
   - Request Body — JSON syntax-highlighted, scrollable
   - Response Body — JSON syntax-highlighted, scrollable

**Data fetching:**
- `useRequestDetail(id)` hook using TanStack Query
- Fetches on panel open and when navigating prev/next
- Headers/body strings are parsed from JSON on the client side

**State:**
- Component-level state in the Requests page: `selectedId: string | null`, `selectedIndex: number`
- No URL change when panel opens/closes

## File Structure

```
console/src/
├── app.tsx                          # BrowserRouter + Routes
├── components/
│   ├── layout/
│   │   ├── app-layout.tsx           # Sidebar + Toolbar + Outlet
│   │   ├── sidebar.tsx              # Collapsible nav
│   │   └── toolbar.tsx              # Time range selector
│   └── ui/
│       ├── status-badge.tsx         # HTTP status color badge
│       ├── finish-badge.tsx         # Finish reason color badge
│       └── collapsible-section.tsx  # Expand/collapse with count badge
├── hooks/
│   ├── use-requests.ts              # TanStack Query: GET /api/calls
│   └── use-request-detail.ts        # TanStack Query: GET /api/calls/{id}
├── lib/
│   ├── utils.ts                     # cn() helper (existing)
│   ├── api.ts                       # fetch wrapper, base URL, response unwrap
│   └── format.ts                    # Date/number formatting helpers
├── pages/
│   └── requests.tsx                 # Table + SlideOverPanel
├── stores/
│   ├── sidebar.ts                   # Zustand: collapsed/expanded
│   └── toolbar.ts                   # Zustand: timeRange, start/end
└── types/
    └── api.ts                       # ApiResponse, CallListItem, CallDetail, CallsPage
```

## Deferred

- Toolbar: granularity picker, dimension filters (provider/model/server_ip), auto-refresh toggle
- Requests page: search bar, status/finish_reason filter dropdowns
- Turn ID link in detail panel (Turns page not built yet)
- All other pages (placeholder only)
- Dark mode toggle (CSS variables are already defined)
