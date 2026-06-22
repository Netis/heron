//! "Services" view reads — aggregate `spans` by `(server_ip, server_port)`
//! plus the service-topology graph. Ports of the DuckDB `query_services` /
//! `query_services_topology` (see `h-storage-duckdb/src/metrics.rs`); same
//! aggregation, percentiles, app classification, and `__clients__` super-node
//! sentinels, translated to ClickHouse dialect.
//!
//! Dialect notes vs the DuckDB original:
//!   * `list_distinct(array_agg(col))[1:N]` → `groupUniqArray(N)(col)` — collects
//!     distinct values directly with a cap, instead of materialising a
//!     potentially huge `groupArray` then deduping. Read straight into
//!     `Vec<String>` (ClickHouse `Array(String)` → `Vec<String>`).
//!   * `quantile_cont(col, 0.95)` → `toFloat64(quantileTDigest(0.95)(col))`
//!     (t-digest is streaming + cheap; `quantileExact` holds every value).
//!     `quantileTDigest` returns `Float32`, hence the `toFloat64`; wrapped in
//!     `toNullable(...)` so an empty group yields `None` not `0`.
//!   * `epoch_ms(MIN/MAX(request_time))` → `toUnixTimestamp64Milli(min/max(
//!     request_time))`.
//!   * Body/header sampling avoids reading the heavy ~2 KB body columns across
//!     the whole window: a cheap inner subquery picks the recent ids per
//!     endpoint (`LIMIT N BY`, no body columns) and the outer fetches bodies for
//!     just those ids (`id IN (...)`). See `fetch_app_samples`.
//!   * No JOINs (project rule). The DuckDB topology JOINs `traces` to
//!     `spans` on the turn's first `call_ids` entry; we reimplement that as
//!     a no-JOIN two-step: read the turns + first call_id, fetch those calls,
//!     then map back in Rust. See `query_services_topology`.

use std::collections::{HashMap, HashSet};

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::{AppError, Result};
use h_storage::classify::{classify_app, extract_server_header};
use h_storage::convert::parse_json_string_list;
use h_storage::query::*;

use crate::client::ch_err;
use crate::sql::time_where;
use crate::ClickHouseBackend;

/// Valid `sort_by` fields for `query_services` — mirrors the DuckDB whitelist
/// exactly. The value is interpolated into `ORDER BY`, so an unknown field is
/// rejected up front rather than reaching the engine.
const VALID_SORT_FIELDS: &[&str] = &[
    "call_count",
    "error_count",
    "total_input_tokens",
    "total_output_tokens",
    "ttft_avg_ms",
    "ttft_p95_ms",
    "e2e_avg_ms",
    "e2e_p95_ms",
    "last_seen_ms",
    "first_seen_ms",
    "server_ip",
    "server_port",
];

/// Per-endpoint app-classification sample. Mirrors the DuckDB `AppSample`
/// helper struct: distinct request paths / finish reasons over the clipped
/// window plus one small header / body blob per field for the classifier.
#[derive(Default)]
struct AppSample {
    request_paths: Vec<String>,
    finish_reasons: Vec<String>,
    sample_response_headers: Option<String>,
    sample_request_headers: Option<String>,
    sample_request_body: Option<String>,
    sample_response_body: Option<String>,
}

/// Row of the cheap dim-sample aggregation (distinct paths / finish reasons per
/// endpoint). `Array(String)` → `Vec<String>` directly.
#[derive(Row, Deserialize)]
struct DimSampleRow {
    server_ip: String,
    server_port: u16,
    request_paths: Vec<String>,
    finish_reasons: Vec<String>,
}

/// Row of the top-N-recent body/header sample query. All four blob columns are
/// `Nullable` (request/response bodies are `Nullable(String)`; headers are
/// non-null but `toNullable(...)`-wrapped so the row type is uniform).
#[derive(Row, Deserialize)]
struct BodySampleRow {
    server_ip: String,
    server_port: u16,
    response_headers: Option<String>,
    request_headers: Option<String>,
    request_body: Option<String>,
    response_body: Option<String>,
}

/// Main per-endpoint aggregation row for `query_services`.
#[derive(Row, Deserialize)]
struct ServiceAggRow {
    server_ip: String,
    server_port: u16,
    models: Vec<String>,
    wire_apis: Vec<String>,
    request_paths: Vec<String>,
    call_count: u64,
    error_count: u64,
    stream_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    ttft_avg_ms: Option<f64>,
    ttft_p95_ms: Option<f64>,
    e2e_avg_ms: Option<f64>,
    e2e_p95_ms: Option<f64>,
    first_seen_ms: i64,
    last_seen_ms: i64,
}

/// Per-endpoint node aggregation row for `query_services_topology` — same
/// grouping as `query_services`, just the columns the graph needs.
#[derive(Row, Deserialize)]
struct NodeAggRow {
    server_ip: String,
    server_port: u16,
    models: Vec<String>,
    request_paths: Vec<String>,
    call_count: u64,
}

/// One turn's proxy metadata + its first call_id, read from `traces`.
/// `call_ids` is the raw JSON-array String column; `first_call_id` is the first
/// element, extracted server-side (`JSONExtractArrayRaw` + `arrayElement`) but
/// re-cleaned in Rust to strip the JSON quoting.
#[derive(Row, Deserialize)]
struct TurnEndpointRow {
    proxy_role: String,
    pair_id: String,
    call_ids: String,
}

/// `(server_ip, server_port, client_ip)` for one llm_call, used to resolve a
/// turn's endpoint via its first call_id (no-JOIN two-step).
#[derive(Row, Deserialize)]
struct CallEndpointRow {
    id: String,
    server_ip: String,
    server_port: u16,
    client_ip: String,
}

impl ClickHouseBackend {
    /// "Services" view — aggregate `spans` by `(server_ip, server_port)`.
    /// Port of the DuckDB `query_services`; see that fn + `StorageBackend::
    /// query_services` for motivation (port is not on `llm_metrics`).
    pub(crate) async fn query_services(&self, query: &ServicesQuery) -> Result<Vec<ServiceRow>> {
        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.eq_ignore_ascii_case("ASC") {
            "ASC"
        } else {
            "DESC"
        };

        // Body / header sampling for app classification — separate, clipped
        // query (keeps the heavy body columns out of the main aggregation).
        let samples = self
            .fetch_app_samples(query.time_range.start_us, query.time_range.end_us)
            .await?;

        let where_sql = time_where(
            "request_time",
            query.time_range.start_us,
            query.time_range.end_us,
        );

        // groupArray collects all values; arrayDistinct dedups; arraySlice caps
        // the list (1-based, same [1:N] semantics as DuckDB). status_code is
        // Nullable so `status_code >= 400` is NULL-safe in ClickHouse; is_stream
        // is Bool. Percentiles wrapped in toNullable so an all-NULL group → None
        // rather than 0. count() → u64; sums cast to UInt64 to match the row type.
        let sql = format!(
            "SELECT
                server_ip,
                server_port,
                groupUniqArray(32)(model)        AS models,
                groupUniqArray(8)(wire_api)      AS wire_apis,
                groupUniqArray(16)(request_path) AS request_paths,
                count()                                                     AS call_count,
                toUInt64(sum(if(status_code >= 400, 1, 0)))                 AS error_count,
                toUInt64(sum(if(is_stream, 1, 0)))                          AS stream_count,
                toUInt64(sum(toUInt64(coalesce(input_tokens, 0))))         AS total_input_tokens,
                toUInt64(sum(toUInt64(coalesce(output_tokens, 0))))        AS total_output_tokens,
                toNullable(avg(ttft_ms))                                    AS ttft_avg_ms,
                toNullable(toFloat64(quantileTDigest(0.95)(ttft_ms)))      AS ttft_p95_ms,
                toNullable(avg(e2e_latency_ms))                             AS e2e_avg_ms,
                toNullable(toFloat64(quantileTDigest(0.95)(e2e_latency_ms))) AS e2e_p95_ms,
                toUnixTimestamp64Milli(min(request_time))                  AS first_seen_ms,
                toUnixTimestamp64Milli(max(request_time))                  AS last_seen_ms
             FROM spans
             WHERE {where_sql}
             GROUP BY server_ip, server_port
             ORDER BY {} {sort_order}
             LIMIT {}",
            query.sort_by, query.limit,
        );

        let agg_rows = self
            .client
            .query(&sql)
            .fetch_all::<ServiceAggRow>()
            .await
            .map_err(|e| ch_err("query_services", e))?;

        let mut rows = Vec::with_capacity(agg_rows.len());
        for r in agg_rows {
            let models = r.models;
            let wire_apis = r.wire_apis;
            // Default request_paths comes from the main window; if the endpoint
            // also has a recent sample, prefer the recent paths.
            let mut request_paths = r.request_paths;

            let sample = samples.get(&(r.server_ip.clone(), r.server_port));
            let finish_reasons = sample.map(|s| s.finish_reasons.clone()).unwrap_or_default();
            if let Some(s) = sample {
                if !s.request_paths.is_empty() {
                    request_paths = s.request_paths.clone();
                }
            }
            let sample_response_headers = sample.and_then(|s| s.sample_response_headers.as_deref());
            let sample_request_headers = sample.and_then(|s| s.sample_request_headers.as_deref());
            let sample_request_body = sample.and_then(|s| s.sample_request_body.as_deref());
            let sample_response_body = sample.and_then(|s| s.sample_response_body.as_deref());

            let server_header = extract_server_header(sample_response_headers);
            let app = classify_app(
                server_header.as_deref(),
                sample_response_headers,
                sample_request_headers,
                &request_paths,
                &finish_reasons,
                &models,
                sample_request_body,
                sample_response_body,
            );

            rows.push(ServiceRow {
                server_ip: r.server_ip,
                server_port: r.server_port,
                models,
                wire_apis,
                request_paths,
                call_count: r.call_count,
                error_count: r.error_count,
                stream_count: r.stream_count,
                total_input_tokens: r.total_input_tokens,
                total_output_tokens: r.total_output_tokens,
                ttft_avg_ms: r.ttft_avg_ms,
                ttft_p95_ms: r.ttft_p95_ms,
                e2e_avg_ms: r.e2e_avg_ms,
                e2e_p95_ms: r.e2e_p95_ms,
                first_seen_ms: r.first_seen_ms,
                last_seen_ms: r.last_seen_ms,
                app,
                server_header,
            });
        }
        Ok(rows)
    }

    /// Per-endpoint app-classification samples. Two cheap queries (both clipped
    /// to the last 24 h — app class doesn't drift over the wider window):
    ///   1. a dim query for distinct paths / finish reasons (`groupUniqArray`),
    ///   2. a body/header sample that fetches the heavy ~2 KB body columns for
    ///      only the 5 most-recent calls per endpoint — the recent ids come from
    ///      an inner `LIMIT N BY` subquery that reads no body columns, and the
    ///      outer reads bodies via `id IN (...)`. (The earlier `ROW_NUMBER`
    ///      window read every row's bodies before filtering — ~60× slower.)
    ///
    /// The shape filter (body length + leading `{`, headers leading `[`) runs in
    /// Rust on the few returned rows, exactly like the DuckDB version.
    async fn fetch_app_samples(
        &self,
        window_start_us: i64,
        window_end_us: i64,
    ) -> Result<HashMap<(String, u16), AppSample>> {
        const SAMPLE_WINDOW_US: i64 = 24 * 60 * 60 * 1_000_000;
        let sample_start_us = std::cmp::max(window_start_us, window_end_us - SAMPLE_WINDOW_US);
        let sample_where = time_where("request_time", sample_start_us, window_end_us);

        let mut out: HashMap<(String, u16), AppSample> = HashMap::new();

        // Dim sample — distinct request paths / finish reasons per endpoint.
        // Cheap (small columns). Cap matches DuckDB: paths[:16], finish[:32].
        let dim_sql = format!(
            "SELECT
                server_ip,
                server_port,
                groupUniqArray(16)(request_path)  AS request_paths,
                groupUniqArray(32)(finish_reason) AS finish_reasons
             FROM spans
             WHERE {sample_where}
             GROUP BY server_ip, server_port"
        );
        let dim_rows = self
            .client
            .query(&dim_sql)
            .fetch_all::<DimSampleRow>()
            .await
            .map_err(|e| ch_err("fetch_app_samples dim", e))?;
        for r in dim_rows {
            out.insert(
                (r.server_ip, r.server_port),
                AppSample {
                    request_paths: r.request_paths,
                    finish_reasons: r.finish_reasons,
                    ..Default::default()
                },
            );
        }

        // Body / header sampling — fetch the heavy body/header columns only for
        // the 5 most-recent calls per endpoint. The recent ids come from a cheap
        // inner subquery that reads NO body columns (`LIMIT N BY` over small
        // columns); the outer fetches bodies for just those ids via `id IN(...)`.
        // This replaces a `ROW_NUMBER` window that materialised every row's
        // bodies before filtering — the window read the full ~2 KB body columns
        // across the whole window (5 s at 1M rows / 37 s at 5M); the two-phase
        // form is ~60× faster. It's an uncorrelated IN-subquery, not a JOIN, so
        // it honours the no-JOIN read rule. Shape filtering happens in Rust below.
        let body_sql = format!(
            "SELECT server_ip, server_port,
                    toNullable(response_headers) AS response_headers,
                    toNullable(request_headers)  AS request_headers,
                    request_body, response_body
             FROM spans
             WHERE id IN (
                SELECT id FROM spans
                WHERE {sample_where}
                ORDER BY request_time DESC
                LIMIT 5 BY server_ip, server_port
             )"
        );
        let body_rows = self
            .client
            .query(&body_sql)
            .fetch_all::<BodySampleRow>()
            .await
            .map_err(|e| ch_err("fetch_app_samples body", e))?;

        // The rows are the (up to) 5 most-recent calls per endpoint; order among
        // them is unspecified. First-match-wins per field — the filter logic is
        // order-independent (any shape-valid recent sample classifies the app).
        for r in body_rows {
            let entry = out.entry((r.server_ip, r.server_port)).or_default();
            if entry.sample_response_headers.is_none() {
                if let Some(s) = r.response_headers.filter(|s| s.starts_with('[')) {
                    entry.sample_response_headers = Some(s);
                }
            }
            if entry.sample_request_headers.is_none() {
                if let Some(s) = r.request_headers.filter(|s| s.starts_with('[')) {
                    entry.sample_request_headers = Some(s);
                }
            }
            if entry.sample_request_body.is_none() {
                if let Some(s) = r
                    .request_body
                    .filter(|s| (100..=32768).contains(&s.len()) && s.starts_with('{'))
                {
                    entry.sample_request_body = Some(s);
                }
            }
            if entry.sample_response_body.is_none() {
                if let Some(s) = r
                    .response_body
                    .filter(|s| (30..=8192).contains(&s.len()) && s.starts_with('{'))
                {
                    entry.sample_response_body = Some(s);
                }
            }
        }

        Ok(out)
    }

    /// Build the service-topology graph for the Path view. Port of the DuckDB
    /// `query_services_topology` — same node set, proxy / inferred / client
    /// edges, and `__clients__` super-node. The DuckDB original resolves a
    /// turn's `(server_ip, server_port)` via a JOIN from `traces` to
    /// `spans` on the turn's first `call_ids` entry; the project's no-JOIN
    /// rule means we instead read the turns + their first call_id, fetch those
    /// calls in one `IN (...)` lookup, and map back in Rust (see below).
    pub(crate) async fn query_services_topology(
        &self,
        query: &ServicesTopologyQuery,
    ) -> Result<ServicesTopology> {
        let start_us = query.time_range.start_us;
        let end_us = query.time_range.end_us;

        // Body / header sampling for app classification — same helper as the
        // table view.
        let samples = self.fetch_app_samples(start_us, end_us).await?;

        // --- Nodes: one per (server_ip, server_port). Reuses the call_count
        // aggregation, just the columns the graph needs.
        let nodes_where = time_where("request_time", start_us, end_us);
        let nodes_sql = format!(
            "SELECT
                server_ip,
                server_port,
                groupUniqArray(32)(model)        AS models,
                groupUniqArray(16)(request_path) AS request_paths,
                count()                                                     AS call_count
             FROM spans
             WHERE {nodes_where}
             GROUP BY server_ip, server_port"
        );
        let node_rows = self
            .client
            .query(&nodes_sql)
            .fetch_all::<NodeAggRow>()
            .await
            .map_err(|e| ch_err("query_services_topology nodes", e))?;

        let mut nodes: Vec<TopologyNode> = Vec::with_capacity(node_rows.len());
        for r in node_rows {
            let models = r.models;
            let mut request_paths = r.request_paths;
            let sample = samples.get(&(r.server_ip.clone(), r.server_port));
            let finish_reasons = sample.map(|s| s.finish_reasons.clone()).unwrap_or_default();
            if let Some(s) = sample {
                if !s.request_paths.is_empty() {
                    request_paths = s.request_paths.clone();
                }
            }
            let sample_response_headers = sample.and_then(|s| s.sample_response_headers.as_deref());
            let sample_request_headers = sample.and_then(|s| s.sample_request_headers.as_deref());
            let sample_request_body = sample.and_then(|s| s.sample_request_body.as_deref());
            let sample_response_body = sample.and_then(|s| s.sample_response_body.as_deref());
            let server_header = extract_server_header(sample_response_headers);
            let app = classify_app(
                server_header.as_deref(),
                sample_response_headers,
                sample_request_headers,
                &request_paths,
                &finish_reasons,
                &models,
                sample_request_body,
                sample_response_body,
            );
            nodes.push(TopologyNode {
                server_ip: r.server_ip,
                server_port: r.server_port,
                app,
                models,
                call_count: r.call_count,
            });
        }

        // --- Resolve each turn's endpoint via its first call_id. The DuckDB
        // query JOINs traces → spans on the first call_ids entry; the
        // no-JOIN rule means we do this as a two-step:
        //   1. read every turn in the window with its proxy role / pair_id and
        //      its first call_id (parsed from the call_ids JSON in Rust),
        //   2. fetch those calls in one IN (...) lookup, build an
        //      id → (server_ip, server_port, client_ip) map,
        //   3. join the two in Rust.
        // DIVERGENCE FROM DUCKDB: the original used an in-SQL JOIN /
        // correlated extract; we materialize the first call_ids set and do a
        // point-lookup batch instead. The resulting (turn, endpoint) pairs are
        // identical.
        //
        // traces is ReplacingMergeTree → FINAL so the latest row wins.
        // Time filter on start_time matches the DuckDB predicate. We pull the
        // first call id server-side via arrayElement(JSONExtract(call_ids,
        // 'Array(String)'), 1); parsing in Rust as a fallback for robustness.
        let turns_where = time_where("start_time", start_us, end_us);
        let turns_sql = format!(
            "SELECT
                JSONExtractString(coalesce(metadata, ''), 'proxy', 'role')    AS proxy_role,
                JSONExtractString(coalesce(metadata, ''), 'proxy', 'pair_id') AS pair_id,
                span_ids                                                       AS call_ids
             FROM traces FINAL
             WHERE {turns_where}"
        );
        let turn_rows = self
            .client
            .query(&turns_sql)
            .fetch_all::<TurnEndpointRow>()
            .await
            .map_err(|e| ch_err("query_services_topology turns", e))?;

        // Per-turn first call_id + role/pair_id. Skip turns with no calls.
        struct TurnInfo {
            proxy_role: String,
            pair_id: String,
            first_call_id: String,
        }
        let mut turn_infos: Vec<TurnInfo> = Vec::with_capacity(turn_rows.len());
        let mut wanted_ids: HashSet<String> = HashSet::new();
        for t in turn_rows {
            let ids = parse_json_string_list(Some(&t.call_ids));
            if let Some(first) = ids.into_iter().next() {
                if first.is_empty() {
                    continue;
                }
                wanted_ids.insert(first.clone());
                turn_infos.push(TurnInfo {
                    proxy_role: t.proxy_role,
                    pair_id: t.pair_id,
                    first_call_id: first,
                });
            }
        }

        // Fetch the endpoints for those first call_ids in one batch.
        let mut endpoint_by_id: HashMap<String, (String, u16, String)> = HashMap::new();
        if !wanted_ids.is_empty() {
            let id_list: Vec<String> = wanted_ids.into_iter().collect();
            let in_list = crate::sql::sql_in_list(&id_list);
            let calls_sql = format!(
                "SELECT id, server_ip, server_port, client_ip \
                 FROM spans WHERE id IN ({in_list})"
            );
            let call_rows = self
                .client
                .query(&calls_sql)
                .fetch_all::<CallEndpointRow>()
                .await
                .map_err(|e| ch_err("query_services_topology call endpoints", e))?;
            for c in call_rows {
                endpoint_by_id.insert(c.id, (c.server_ip, c.server_port, c.client_ip));
            }
        }

        // --- Proxy edges: pair each proxy_in turn's endpoint with the
        // proxy_out sibling's, grouped by pair_id. Mirrors the DuckDB
        // turn_endpoint self-join: a→b where a.pair_id == b.pair_id,
        // a.turn != b.turn, a.role == proxy_in, b.role == proxy_out,
        // pair_id non-empty, and from != to (drop dup-capture self-pairs).
        // Counted by number of (a,b) turn pairs, then aggregated by endpoint
        // quad — matching DuckDB's COUNT(*) GROUP BY both endpoints.
        let mut by_pair_in: HashMap<&str, Vec<(String, u16)>> = HashMap::new();
        let mut by_pair_out: HashMap<&str, Vec<(String, u16)>> = HashMap::new();
        for ti in &turn_infos {
            if ti.pair_id.is_empty() {
                continue;
            }
            let ep = match endpoint_by_id.get(&ti.first_call_id) {
                Some((ip, port, _client)) => (ip.clone(), *port),
                None => continue,
            };
            if ti.proxy_role == "proxy_in" {
                by_pair_in.entry(ti.pair_id.as_str()).or_default().push(ep);
            } else if ti.proxy_role == "proxy_out" {
                by_pair_out.entry(ti.pair_id.as_str()).or_default().push(ep);
            }
        }
        // Aggregate (from_ip, from_port, to_ip, to_port) → turn_count, the
        // self-join COUNT(*): for each pair_id, every proxy_in × every
        // proxy_out is one pairing.
        let mut proxy_counts: HashMap<(String, u16, String, u16), u64> = HashMap::new();
        for (pair_id, ins) in &by_pair_in {
            if let Some(outs) = by_pair_out.get(pair_id) {
                for (fi, fp) in ins {
                    for (ti, tp) in outs {
                        // Drop same-endpoint pairs (multi-interface dup capture,
                        // not a real proxy hop).
                        if fi == ti && fp == tp {
                            continue;
                        }
                        *proxy_counts
                            .entry((fi.clone(), *fp, ti.clone(), *tp))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
        let proxy_edges: Vec<TopologyEdge> = proxy_counts
            .into_iter()
            .map(|((fi, fp, ti, tp), c)| TopologyEdge {
                from_ip: fi,
                from_port: fp,
                to_ip: ti,
                to_port: tp,
                turn_count: c,
                kind: "proxy".to_string(),
            })
            .collect();

        // --- Inbound entry edges, grouped by (caller_ip, to_ip, to_port).
        // DuckDB excludes proxy_out turns (their inbound side is the proxy hop,
        // already covered above). Resolve each caller_ip to an originating
        // service (litellm > proxy-ish > most-active), else fall through to a
        // synthetic __clients__ edge. Same logic as the DuckDB resolve_caller.
        let mut entry_counts: HashMap<(String, String, u16), u64> = HashMap::new();
        for ti in &turn_infos {
            if ti.proxy_role == "proxy_out" {
                continue;
            }
            if let Some((to_ip, to_port, caller_ip)) = endpoint_by_id.get(&ti.first_call_id) {
                *entry_counts
                    .entry((caller_ip.clone(), to_ip.clone(), *to_port))
                    .or_insert(0) += 1;
            }
        }

        // Per-IP service index for caller resolution.
        let mut services_by_ip: HashMap<&str, Vec<&TopologyNode>> = HashMap::new();
        for n in &nodes {
            services_by_ip
                .entry(n.server_ip.as_str())
                .or_default()
                .push(n);
        }
        let app_of: HashMap<(String, u16), Option<String>> = nodes
            .iter()
            .map(|n| ((n.server_ip.clone(), n.server_port), n.app.clone()))
            .collect();
        let is_proxy_app = |app: Option<&str>| {
            matches!(app, Some("litellm") | Some("haproxy") | Some("nginx"))
        };
        let resolve_caller = |caller_ip: &str, to_ip: &str, to_port: u16| -> Option<(String, u16)> {
            // If the TARGET is itself a proxy, inbound calls are real clients,
            // not another local service forwarding.
            let target_app = app_of
                .get(&(to_ip.to_string(), to_port))
                .and_then(|a| a.as_deref());
            if is_proxy_app(target_app) {
                return None;
            }
            let candidates = services_by_ip.get(caller_ip)?;
            let usable: Vec<&&TopologyNode> = candidates
                .iter()
                .filter(|n| !(n.server_ip == to_ip && n.server_port == to_port))
                .collect();
            if usable.is_empty() {
                return None;
            }
            // 1) Prefer litellm.
            if let Some(n) = usable.iter().find(|n| n.app.as_deref() == Some("litellm")) {
                return Some((n.server_ip.clone(), n.server_port));
            }
            // 2) Else any proxy-ish app.
            if let Some(n) = usable.iter().find(|n| is_proxy_app(n.app.as_deref())) {
                return Some((n.server_ip.clone(), n.server_port));
            }
            // 3) Else most-active service on that IP.
            let n = usable
                .iter()
                .max_by_key(|n| n.call_count)
                .expect("usable non-empty");
            Some((n.server_ip.clone(), n.server_port))
        };

        // Dedupe inferred edges against proxy_pair edges already produced.
        let proxy_pair_set: HashSet<(String, u16, String, u16)> = proxy_edges
            .iter()
            .map(|e| (e.from_ip.clone(), e.from_port, e.to_ip.clone(), e.to_port))
            .collect();

        let mut inferred_dedup: HashMap<(String, u16, String, u16), u64> = HashMap::new();
        let mut client_dedup: HashMap<(String, u16), u64> = HashMap::new();
        for ((caller_ip, to_ip, to_port), turn_count) in entry_counts {
            match resolve_caller(&caller_ip, &to_ip, to_port) {
                Some((from_ip, from_port)) => {
                    // Suppress if the pair sweeper already covered this hop.
                    if proxy_pair_set.contains(&(
                        from_ip.clone(),
                        from_port,
                        to_ip.clone(),
                        to_port,
                    )) {
                        continue;
                    }
                    *inferred_dedup
                        .entry((from_ip, from_port, to_ip, to_port))
                        .or_insert(0) += turn_count;
                }
                None => {
                    *client_dedup.entry((to_ip, to_port)).or_insert(0) += turn_count;
                }
            }
        }

        let inferred_edges: Vec<TopologyEdge> = inferred_dedup
            .into_iter()
            .map(|((fi, fp, ti, tp), c)| TopologyEdge {
                from_ip: fi,
                from_port: fp,
                to_ip: ti,
                to_port: tp,
                turn_count: c,
                kind: "inferred".to_string(),
            })
            .collect();
        let client_edges: Vec<TopologyEdge> = client_dedup
            .into_iter()
            .map(|((ti, tp), c)| TopologyEdge {
                from_ip: "__clients__".to_string(),
                from_port: 0,
                to_ip: ti,
                to_port: tp,
                turn_count: c,
                kind: "client".to_string(),
            })
            .collect();

        let mut edges = proxy_edges;
        edges.extend(inferred_edges);
        edges.extend(client_edges);

        // Synthetic __clients__ node — total = sum of every client edge.
        let client_total: u64 = edges
            .iter()
            .filter(|e| e.kind == "client")
            .map(|e| e.turn_count)
            .sum();
        if client_total > 0 {
            nodes.push(TopologyNode {
                server_ip: "__clients__".to_string(),
                server_port: 0,
                app: Some("clients".to_string()),
                models: Vec::new(),
                call_count: client_total,
            });
        }

        Ok(ServicesTopology { nodes, edges })
    }
}
