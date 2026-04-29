# Pipeline Health 页面设计

**状态：** draft
**作者：** brainstorming session
**日期：** 2026-04-28

## 1. 目标

把 `MetricsReporter` 已经在打日志的内部诊断指标（`pkts_received`、`q_raw_pkts(97%)`、`tcp_ooo_dropped`…）以可视化的形式呈现出来，让开发者**一屏看清** pipeline 当前是否健康、瓶颈在哪、记录在哪一段被丢/被过滤。

## 2. 非目标（明确不做）

- **不做存储 / 历史趋势 / sparkline**：仅 L1 实时快照。后续可在 `internal_metrics` 表上扩展，本次不写。
- **不做告警 / Webhook / Email 通知**。
- **不做 Prometheus `/metrics` 端点**：可作为后续独立扩展，不在本次范围。
- **不动现有的 `tracing::info!` 日志输出**：日志保留作为运维侧观测渠道，独立于本页。
- **不在前端做"长会话级 ring buffer"**：仅靠 TanStack Query 缓存上一帧用于算 delta，不做 N 帧累积。

## 3. 架构总览

```
                 已存在：
   pipeline ─► MetricsSystem ─► MetricsSvc::snapshot() ─► tracing::info!
                                       │
                                       ▼
                                 GET /api/internal-metrics    （新，始终注册）
                                       │
                                       ▼
                              React /pipeline-health 页面
                              (TanStack Query 轮询 + 客户端算 delta)
                                       ▲
                                       │
                              GET /api/server-info             （新，控制 nav 是否露出）
```

要点：
- 后端 API **完全无状态**：每次请求返回当前快照。delta 由前端基于上一帧自算。
- API 始终注册；UI 是否展示由配置控制。
- 现有 `MetricsReporter` 不动，日志继续打。

## 4. 后端改动

### 4.1 配置

`server/config/default.toml`：

```toml
[internal_metrics]
enabled = true             # 不变：reporter 日志开关
interval_secs = 10         # 不变：日志间隔

[console.features]
pipeline_health = false    # 新增：Console 是否展示 Pipeline Health 页（默认关）
```

`ts-common/src/config.rs`：在 `Config` 上加 `pub console: ConsoleConfig`，`ConsoleConfig { features: ConsoleFeatures }`，`ConsoleFeatures { pipeline_health: bool }`。整体跟 `InternalMetricsConfig` 一样的 deserializer 模式。

### 4.2 API 路由

`ts-api/src/lib.rs::router()` 当前签名：

```rust
pub fn router(storage: Arc<dyn StorageBackend>) -> Router
```

扩展为：

```rust
pub fn router(
    storage: Arc<dyn StorageBackend>,
    metrics: ApiMetricsContext,
    server_info: ServerInfoContext,
) -> Router
```

其中：

```rust
/// 暴露给 /api/internal-metrics 的指标视图。
pub struct ApiMetricsContext {
    /// 每个 pipeline 一份 (name, MetricsSvc)。
    pub pipelines: Vec<(String, Arc<MetricsSvc>)>,
    /// 跨 pipeline 共享（目前是 storage 层）的 MetricsSvc。
    pub global: Arc<MetricsSvc>,
}

/// /api/server-info 反映给前端的 server 元信息。
pub struct ServerInfoContext {
    pub version: &'static str,         // env!("CARGO_PKG_VERSION")
    pub console_features: ConsoleFeatures,
}
```

`app/tokenscope/src/main.rs::run()` 在 build 完 `pipeline_reporter_handles` / `global_reporter_handle` 之后顺便构造 `ApiMetricsContext`，连同 `ServerInfoContext` 一起喂给 `router()`。

#### 4.2.1 `GET /api/internal-metrics`

**始终注册。** 请求方式：`GET`，无参数。

返回 JSON：

```json
{
  "ts": 1714291928,
  "pipelines": [
    {
      "name": "default",
      "metrics": [
        { "name": "pkts_received",       "group": "capture",  "kind": "counter", "value": 12401 },
        { "name": "pkts_dropped_kernel", "group": "capture",  "kind": "counter", "value": 0 },
        { "name": "q_raw_pkts",          "group": "protocol", "kind": "gauge",   "value": 4000, "capacity": 4096 },
        { "name": "flows_active",        "group": "protocol", "kind": "gauge",   "value": 3 }
      ]
    }
  ],
  "global": {
    "metrics": [
      { "name": "buf_calls",     "group": "storage", "kind": "counter", "value": 87 },
      { "name": "flushed_calls", "group": "storage", "kind": "counter", "value": 87 },
      { "name": "q_calls",       "group": "storage", "kind": "gauge",   "value": 2, "capacity": 1024 }
    ]
  }
}
```

字段语义：
- `ts`：服务端 unix 秒。前端用它算"距离上一帧多少秒"，得到 per-second 速率。
- 每条 metric 自带 `group / kind / capacity`，前端不需要硬编码任何指标元数据 —— 加新指标只改后端 enum，前端自动跟上。
- `kind = "counter"` 或 `"gauge"`。`capacity` 仅 `gauge` 且为 capped queue 时存在。
- `name` 用 `Metric::short_name()`（已稳定的 grep-friendly 名字）。

实现：循环 `MetricsSvc::snapshot()` 的结果，逐个 `Metric` 调 `metric.spec()` 拿元数据，组装。`capacities()` 已有现成接口。

错误：永远不会失败。指标系统是内存读取。

#### 4.2.2 `GET /api/server-info`

**始终注册。** 请求方式：`GET`，无参数。

返回 JSON：

```json
{
  "version": "0.1.0",
  "console": {
    "features": {
      "pipeline_health": false
    }
  }
}
```

`console.features` 直接镜像 `[console.features]` toml 段（toml 段名 ⇿ JSON key 一一对应）。前端可以根据 `console.features.pipeline_health` 决定 nav 是否渲染。

字段约定：
- 任何 `console.features.*` 字段未来都默认 `false`。前端见到 `undefined` 当 `false` 处理，避免老前端配新后端时多渲染未知 feature。

### 4.3 测试

`ts-api`：
- `/api/internal-metrics` 路由集成测试：用一个最小 `MetricsSystem`（注册 1 个 counter + 1 个 capped gauge），起 router，curl，断言 JSON 形状。
- `/api/server-info` 单元测试：构造 `ServerInfoContext`，断言序列化结构。

`ts-common`：
- `ConsoleConfig` 反序列化测试（带/不带 `[console.features]` 段都要正确解析）。

## 5. 前端改动

### 5.1 路由 + 导航

`console/src/app.tsx`：在 `routes` 加 `<Route path="/pipeline-health" element={<PipelineHealthPage />} />`。

`console/src/components/layout/sidebar.tsx`：`navItems` 数组加：

```ts
{ to: "/pipeline-health", icon: Activity, label: "Pipeline Health", devOnly: true }
```

`devOnly` 字段在 sidebar 渲染时过滤 —— 读 `useServerInfo()` 拿到的 `console.features.pipeline_health`，false 时跳过该项。

### 5.2 server-info 探测

新建 `console/src/hooks/use-server-info.ts`：

```ts
export function useServerInfo() {
  return useQuery({
    queryKey: ["server-info"],
    queryFn: () => apiFetch<ServerInfo>("/api/server-info"),
    staleTime: Infinity,         // 启动一次就够
  })
}
```

在 `app.tsx` 顶层调用一次（loader），结果存 TanStack Query 缓存，sidebar / page guard 自取。

`PipelineHealthPage` 入口处 guard：`server-info` 没拿到时显示 spinner；`features.pipeline_health` 为 false 时 `<Navigate to="/" replace />`。

### 5.3 数据 hook

`console/src/hooks/use-internal-metrics.ts`：

```ts
type MetricSnapshot = {
  ts: number
  pipelines: Array<{
    name: string
    metrics: Array<{
      name: string
      group: "capture"|"protocol"|"llm"|"turn"|"metrics"|"storage"
      kind: "counter"|"gauge"
      value: number
      capacity?: number
    }>
  }>
  global: { metrics: Array<...> }   // 同上
}

export function useInternalMetrics() {
  const intervalMs = usePipelineHealthStore(s => s.intervalMs)  // 1000/2000/5000/null(pause)
  return useQuery({
    queryKey: ["internal-metrics"],
    queryFn: () => apiFetch<MetricSnapshot>("/api/internal-metrics"),
    refetchInterval: intervalMs ?? false,
  })
}
```

`usePipelineHealthStore`（Zustand）只放页面级偏好：`intervalMs: number | null`、`selectedPipeline: string | null`。

### 5.4 客户端 delta 计算

页面组件持有 `usePrev<MetricSnapshot>()`，每次 render 计算：

```ts
const deltaPerSec = (current.value - prev.value) / (current.ts - prev.ts)
```

首帧 prev 为 undefined → delta 全部显示 `—`。

### 5.5 页面结构

整体一个 `PipelineHealthPage` 组件，竖直堆叠 5 段：

```
┌──────────────────────────────────────────────────────────────┐
│ Header                                                        │
│  Pipeline: [default ▾]   Health: [⚠ 1 warning]  Refresh: 2s ▾ │
├──────────────────────────────────────────────────────────────┤
│ ① Backpressure 拓扑                                          │
│    queue 沿 pipeline 横排，水位条 + 百分比，超阈值变色        │
├──────────────────────────────────────────────────────────────┤
│ ② Throughput 漏斗                                            │
│    pkts_received → ... → flushed_calls 的 stage 流量         │
│    每行：标签 + 横条（按数量比例）+ 数值                      │
│    每个有掉量的 stage 下方注释 "↳ −28 (not_ip 23, ...)"     │
├──────────────────────────────────────────────────────────────┤
│ ③ State gauges                                               │
│    flows_active / turns_active / tcp_ooo_buffered / ...      │
│    KPI 卡网格                                                 │
├──────────────────────────────────────────────────────────────┤
│ ④ 错误红榜                                                    │
│    任何累计值非零 OR delta 非零的 error counter，红/黄高亮    │
├──────────────────────────────────────────────────────────────┤
│ ⑤ All Metrics 全量表（默认折叠）                             │
│    所有 metric 一张表，可按 group 过滤、列排序、"only ⚠"     │
└──────────────────────────────────────────────────────────────┘
```

#### 5.5.1 Header

- **Pipeline 选择器**：dropdown，列出所有 `pipelines[].name`。`selectedPipeline === null` 时回退到第一个 pipeline。当只有 1 个 pipeline 时，dropdown 退化为只读文本。下面所有 section 的"当前 pipeline 数据"都基于这里选中的那个；`global`（storage 层）始终一起参与渲染（漏斗最后一段、错误红榜、全量表都会用到）。
- **Health pill**：根据健康规则（§6）显示 `Healthy` / `⚠ N warnings` / `✗ N critical`。
- **Refresh 控件**：分段控件 `1s / 2s / 5s / Pause`。默认 2s。Pause 时停止轮询，最后一帧保留显示，badge 提示 "paused at 14:32:08"。

#### 5.5.2 Section ① Backpressure 拓扑

输入：当前 pipeline 中 `kind=gauge` 且 `capacity != null` 的所有指标 + global 中同类。

排版：横向 flex 行，按 pipeline 数据流顺序固定排列：

```
q_raw_pkts → q_parsed_pkts → q_http_parse_events → q_http_joiner_events
  → q_agent_calls → q_llm_events → (q_calls / q_turns / q_metrics / q_exchanges)
```

最后 4 个 storage queue 合成一格"Storage queues"展开。

每格渲染：name + `value/capacity (pct%)` + 4px 横条。pct 阈值见 §6。

#### 5.5.3 Section ② Throughput 漏斗

数据源：当前选中 pipeline 的所有 metric **+** `global.metrics`（最后一段 `flushed_calls` 来自 global）。

固定显示以下"漏斗节点"，按顺序：

| label | 取值 |
|---|---|
| pkts_received | `pkts_received` |
| pkts_parsed | `pkts_parsed` |
| http_exchanges_joined | `http_exchanges_joined` |
| wires_detected | `wires_detected` |
| calls_with_agent | `calls_with_agent` |
| calls_ingested | `calls_ingested` |
| turns_completed | `turns_completed` |
| flushed_calls | `flushed_calls` |

每行：左侧 label，中间横条（宽度 = `value / pkts_received` 比例），右侧 value 文本。

每个**已知有掉量来源**的 stage 下方紧跟一行 `↳ ... ` 直接展开掉量明细：

| stage | 掉量明细行（来自其它 metric） |
|---|---|
| pkts_parsed | `−{pkts_dropped_not_ip + pkts_dropped_not_tcp + pkts_dropped_malformed} (not_ip {N}, not_tcp {N}, malformed {N})` |
| http_exchanges_joined | `−{http_exchanges_unpaired + http_exchanges_expired} (unpaired {N}, expired {N})` |
| wires_detected | `subset that matched LLM wire-API; rest are wires_ignored {N}` |
| calls_with_agent | `−{calls_without_agent}` |
| calls_ingested | `−{calls_dropped_late}, +{calls_auxiliary} auxiliary` |
| turns_completed | `fan-in (multiple calls per turn — not a drop)` |
| flushed_calls | `buf_calls − flushed_calls = {N} not yet flushed` |

#### 5.5.4 Section ③ State gauges

KPI 卡网格（5 列），固定包含：

- `flows_active`、`turns_active`、`tcp_ooo_buffered`、`flows_expired`（counter，但语义是状态）、`heartbeats_emitted`、`batches_received`、`http_resyncs`

#### 5.5.5 Section ④ 错误红榜

筛选 + 渲染逻辑：遍历当前 pipeline + global 所有 metric，命中以下任一即列出，按命中级别（critical/warning）排序：

**critical（红）：**
- `pkts_dropped_kernel`（任何累计值非零或 delta > 0）
- `flush_errors / read_errors / dump_errors / batches_dropped_zmq`（同上）

**warning（黄）：**
- `tcp_ooo_dropped / http_resyncs / turns_discarded_no_user_start / calls_dropped_late / heartbeats_dropped`（累计 > 0 或 delta > 0）

每条卡片：metric 名 + 当前值 + delta + 简短解释（hardcoded 文案）。

无任何条目时显示绿色 "No errors" 提示。

#### 5.5.6 Section ⑤ All Metrics 全量表

`<details>` 默认折叠。展开后：

- 顶部 chip：`all / capture / protocol / llm / turn / metrics / storage` 单选过滤；右侧 `⚠ only` 切换。
- 表头列：`group | metric | kind | value | Δ/s | cap%`，每列可排序。
- 行渲染：counter 显示 `value (+delta/s)`；gauge with cap 显示 `value/cap (pct%)`；普通 gauge 显示 `value`。Gauge 不显示 Δ（无意义）。
- 行高亮规则：仅当该指标命中 §6 critical/warning 列表中的某一条时整行变红/黄。普通正向计数器（如 `pkts_received` 持续增长）不算异常，不高亮。

### 5.6 测试

- `useInternalMetrics` hook 单元测试（mock fetch，断言 query key）。
- 健康规则纯函数 `computeHealth(snapshot, prevSnapshot)`：单测命中各档阈值。
- 漏斗 stage 渲染单测：给定 snapshot 断言宽度比例 + 掉量注释。
- E2E：起 server with `pipeline_health = true`，访问 `/pipeline-health`，断言核心 DOM；with `pipeline_health = false`，访问同 URL 应跳到 `/`。

## 6. 健康判定规则

写死在前端纯函数里（路径 `console/src/lib/pipeline-health.ts`），不进配置。

### 6.1 Critical（红）—— 任一命中

- `pkts_dropped_kernel` delta > 0
- 任何 capped gauge `value / capacity ≥ 0.95`
- `flush_errors`、`read_errors`、`dump_errors`、`batches_dropped_zmq` 任意 delta > 0

### 6.2 Warning（黄）—— 任一命中且无 critical

- 任何 capped gauge `value / capacity ≥ 0.90`
- `tcp_ooo_dropped`、`http_resyncs`、`turns_discarded_no_user_start`、`calls_dropped_late`、`heartbeats_dropped` 任意 delta > 0
- 上述任何 error counter 累计值 > 0（即使 delta = 0 —— 提示历史曾经发生过）

### 6.3 Healthy（绿）

以上都不命中。

### 6.4 阈值不进配置的理由

`90/95` 是经验值；用户调整的需求要在真实部署中观察后才会出现。先 hardcode + 注释，后续真有需求再抽配置。YAGNI。

## 7. 模块边界与单元

| 单元 | 在哪 | 干什么 | 依赖 |
|---|---|---|---|
| `ApiMetricsContext` / `ServerInfoContext` | `ts-api/src/lib.rs` | router 注入参数，承载 `MetricsSvc` 集合和 server 元信息 | `ts-common`, `MetricsSvc` |
| `routes/internal_metrics.rs` | `ts-api/src/routes/` | `GET /api/internal-metrics` handler，把 snapshot 渲染成 JSON | `ApiMetricsContext` |
| `routes/server_info.rs` | `ts-api/src/routes/` | `GET /api/server-info` handler | `ServerInfoContext` |
| `ConsoleConfig` | `ts-common/src/config.rs` | 反序列化 `[console.features]` | serde |
| `useServerInfo` | `console/src/hooks/` | 一次性拉取 server-info，TanStack Query 缓存 | TanStack Query |
| `useInternalMetrics` | `console/src/hooks/` | 按当前 interval 轮询 `/api/internal-metrics` | TanStack Query, `usePipelineHealthStore` |
| `usePipelineHealthStore` | `console/src/stores/` | 持有页面级偏好（interval, selectedPipeline） | Zustand |
| `pipeline-health.ts` | `console/src/lib/` | 纯函数：`computeHealth`、`computeFunnelStages`、`isCriticalMetric` | 无 |
| `PipelineHealthPage` | `console/src/pages/` | 页面壳，组合 5 段 section | hooks + lib |
| `BackpressureSection`、`FunnelSection`、`StateGaugesSection`、`ErrorListSection`、`AllMetricsTable` | `console/src/components/pipeline-health/` | 各段独立组件 | snapshot 数据 |

每段 section 接收 `(currentSnapshot, prevSnapshot)`，无副作用，纯渲染。

## 8. Open questions / future work

- **Prometheus `/metrics` 端点**：跟本页解耦的独立后续扩展。当用户把 TokenScope 接进自己的 observability 栈时再做。
- **持久化 + 历史趋势**：`internal_metrics` 表 + sparkline + 时间范围回看。本次 explicitly 砍掉，待真有需求再立项。
- **多 pipeline 的健康聚合**：当前 header 只反映当前选中的 pipeline。如果用户跑 N 个 pipeline 想要"整体健康总览"，可以在 sidebar nav 上加一个汇总徽标，留作后续。
- **错误明细的可点击追溯**：例如点击 `pkts_dropped_kernel` 跳到 capture 配置 / 文档解释。属于 polish，后续。
