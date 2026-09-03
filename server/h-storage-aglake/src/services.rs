//! Services table + topology graph, aggregated over spans.
//!
//! # App classification diverges from the SQL backends
//!
//! They sample a handful of recent bodies per endpoint at read time and run
//! `classify_app` once over that sample. Here every span is classified at
//! write time (`app_hint` in [`crate::rows`]) and the read takes the **most
//! common** hint per endpoint. Same classifier, different input: theirs sees
//! one recent example, this sees every call and picks the majority. The
//! results agree in practice and the majority answer is the more stable one,
//! but they are not guaranteed byte-identical — an endpoint whose traffic
//! changed shape mid-window can classify differently.
//!
//! The reason for the difference is structural: bodies live in their own
//! index here, so sampling them would cost an extra query against the largest
//! index in the deployment purely to label a row.
//!
//! # `values()` is unbounded
//!
//! ClickHouse caps its distinct-value lists in SQL (`groupUniqArray(32)`).
//! SPL's `values()` has no such argument, so a high-cardinality endpoint
//! collects every distinct value server-side before this code can truncate.
//! `--max-agg-mem-mb` is the backstop, which turns the pathological case into
//! an error rather than an OOM. The lists are truncated here to the same caps
//! the SQL backends use so the returned rows match.

use std::collections::{HashMap, HashSet};

use h_common::error::{AppError, Result};
use h_storage::query::*;

use crate::client::Row;
use crate::rows::{ST_SPAN, ST_TRACE};
use crate::spl::{self, Search};
use crate::AglakeBackend;

const VALID_SORT_FIELDS: &[&str] = &[
    "call_count",
    "error_count",
    "total_input_tokens",
    "total_output_tokens",
    "ttft_avg_ms",
    "e2e_avg_ms",
    "last_seen_ms",
];

/// Caps matching the SQL backends' `groupUniqArray(N)` / `[:N]`.
const MAX_MODELS: usize = 32;
const MAX_WIRE_APIS: usize = 8;
const MAX_REQUEST_PATHS: usize = 16;

/// Ceiling on turns considered when building the topology graph.
///
/// The graph needs every turn's first call resolved to an endpoint, which is
/// an id-list lookup whose size is the turn count. At 10^5–10^6 turns that is
/// tens of megabytes of query string and minutes of work, so past this the
/// graph is truncated and says so rather than timing out.
const MAX_TOPOLOGY_TURNS: usize = 10_000;

/// `(app_hint, server_header)` for one endpoint.
type AppLabel = (Option<String>, Option<String>);

/// The winning `(count, app_hint, server_header)` for one endpoint while the
/// majority vote is being tallied.
type AppVote = (u64, Option<String>, Option<String>);

/// One endpoint's aggregate.
struct Endpoint {
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

impl AglakeBackend {
    /// Per-endpoint aggregate over the spans index. Shared by the table view
    /// and the topology nodes.
    async fn service_endpoints(&self, range: &TimeRange) -> Result<Vec<Endpoint>> {
        let s = Search::new(&self.ix.spans, ST_SPAN);
        let spl_q = format!(
            "{} | stats \
               values(model) as models, values(wire_api) as wire_apis, \
               values(request_path) as request_paths, \
               count as call_count, sum(err) as error_count, sum(strm) as stream_count, \
               sum(input_tokens) as total_input_tokens, \
               sum(output_tokens) as total_output_tokens, \
               avg(ttft_ms) as ttft_avg_ms, perc95(ttft_ms) as ttft_p95_ms, \
               avg(e2e_latency_ms) as e2e_avg_ms, perc95(e2e_latency_ms) as e2e_p95_ms, \
               min(ts_us) as first_us, max(ts_us) as last_us \
             by server_ip, server_port \
             | table server_ip, server_port, models, wire_apis, request_paths, \
               call_count, error_count, stream_count, total_input_tokens, \
               total_output_tokens, ttft_avg_ms, ttft_p95_ms, e2e_avg_ms, e2e_p95_ms, \
               first_us, last_us",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(range.start_us),
                &spl::epoch_secs(range.end_us),
            )
            .await?
            .rows();

        Ok(rows
            .into_iter()
            .map(|r| {
                let mut models = str_list(&r, "models");
                let mut wire_apis = str_list(&r, "wire_apis");
                let mut request_paths = str_list(&r, "request_paths");
                models.truncate(MAX_MODELS);
                wire_apis.truncate(MAX_WIRE_APIS);
                request_paths.truncate(MAX_REQUEST_PATHS);
                Endpoint {
                    server_ip: string(&r, "server_ip"),
                    server_port: num(&r, "server_port").unwrap_or(0.0) as u16,
                    models,
                    wire_apis,
                    request_paths,
                    call_count: int(&r, "call_count"),
                    error_count: int(&r, "error_count"),
                    stream_count: int(&r, "stream_count"),
                    total_input_tokens: int(&r, "total_input_tokens"),
                    total_output_tokens: int(&r, "total_output_tokens"),
                    ttft_avg_ms: num(&r, "ttft_avg_ms"),
                    ttft_p95_ms: num(&r, "ttft_p95_ms"),
                    e2e_avg_ms: num(&r, "e2e_avg_ms"),
                    e2e_p95_ms: num(&r, "e2e_p95_ms"),
                    first_seen_ms: num(&r, "first_us").unwrap_or(0.0) as i64 / 1000,
                    last_seen_ms: num(&r, "last_us").unwrap_or(0.0) as i64 / 1000,
                }
            })
            .collect())
    }

    /// Most common `(app_hint, server_header)` per endpoint.
    ///
    /// A separate small aggregation rather than `values()` on the main query:
    /// picking the majority needs counts, and `values()` only returns the
    /// distinct set — from which "first alphabetically" would be the only
    /// available choice, which is not a classification.
    async fn app_by_endpoint(&self, range: &TimeRange) -> Result<HashMap<(String, u16), AppLabel>> {
        let s = Search::new(&self.ix.spans, ST_SPAN);
        let spl_q = format!(
            "{} | stats count as n by server_ip, server_port, app_hint, server_header \
             | table server_ip, server_port, app_hint, server_header, n",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(range.start_us),
                &spl::epoch_secs(range.end_us),
            )
            .await?
            .rows();

        let mut best: HashMap<(String, u16), AppVote> = HashMap::new();
        for r in rows {
            let key = (
                string(&r, "server_ip"),
                num(&r, "server_port").unwrap_or(0.0) as u16,
            );
            let n = int(&r, "n");
            let app = r
                .get("app_hint")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let hdr = r
                .get("server_header")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let e = best.entry(key).or_insert((0, None, None));
            if n > e.0 {
                *e = (n, app, hdr);
            }
        }
        Ok(best
            .into_iter()
            .map(|(k, (_, app, hdr))| (k, (app, hdr)))
            .collect())
    }

    pub(crate) async fn query_services(&self, query: &ServicesQuery) -> Result<Vec<ServiceRow>> {
        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let (endpoints, apps) = tokio::try_join!(
            self.service_endpoints(&query.time_range),
            self.app_by_endpoint(&query.time_range),
        )?;

        let mut rows: Vec<ServiceRow> = endpoints
            .into_iter()
            .map(|e| {
                let (app, server_header) = apps
                    .get(&(e.server_ip.clone(), e.server_port))
                    .cloned()
                    .unwrap_or((None, None));
                ServiceRow {
                    server_ip: e.server_ip,
                    server_port: e.server_port,
                    models: e.models,
                    wire_apis: e.wire_apis,
                    request_paths: e.request_paths,
                    call_count: e.call_count,
                    error_count: e.error_count,
                    stream_count: e.stream_count,
                    total_input_tokens: e.total_input_tokens,
                    total_output_tokens: e.total_output_tokens,
                    ttft_avg_ms: e.ttft_avg_ms,
                    ttft_p95_ms: e.ttft_p95_ms,
                    e2e_avg_ms: e.e2e_avg_ms,
                    e2e_p95_ms: e.e2e_p95_ms,
                    first_seen_ms: e.first_seen_ms,
                    last_seen_ms: e.last_seen_ms,
                    app,
                    server_header,
                }
            })
            .collect();

        let key = |r: &ServiceRow| -> f64 {
            match query.sort_by.as_str() {
                "call_count" => r.call_count as f64,
                "error_count" => r.error_count as f64,
                "total_input_tokens" => r.total_input_tokens as f64,
                "total_output_tokens" => r.total_output_tokens as f64,
                "ttft_avg_ms" => r.ttft_avg_ms.unwrap_or(f64::NEG_INFINITY),
                "e2e_avg_ms" => r.e2e_avg_ms.unwrap_or(f64::NEG_INFINITY),
                _ => r.last_seen_ms as f64,
            }
        };
        let asc = query.sort_order.eq_ignore_ascii_case("ASC");
        rows.sort_by(|a, b| {
            let ord = key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal);
            let ord = if asc { ord } else { ord.reverse() };
            ord.then_with(|| (&a.server_ip, a.server_port).cmp(&(&b.server_ip, b.server_port)))
        });
        rows.truncate(query.limit as usize);
        Ok(rows)
    }

    pub(crate) async fn query_services_topology(
        &self,
        query: &ServicesTopologyQuery,
    ) -> Result<ServicesTopology> {
        let (endpoints, apps) = tokio::try_join!(
            self.service_endpoints(&query.time_range),
            self.app_by_endpoint(&query.time_range),
        )?;

        let nodes: Vec<TopologyNode> = endpoints
            .into_iter()
            .map(|e| {
                let app = apps
                    .get(&(e.server_ip.clone(), e.server_port))
                    .and_then(|(a, _)| a.clone());
                TopologyNode {
                    server_ip: e.server_ip,
                    server_port: e.server_port,
                    app,
                    models: e.models,
                    call_count: e.call_count,
                }
            })
            .collect();

        // Each turn's first call, resolved to the endpoint it hit. Bounded —
        // see MAX_TOPOLOGY_TURNS.
        let turns = self.topology_turns(&query.time_range).await?;
        let wanted: Vec<String> = turns
            .iter()
            .map(|t| t.first_span_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let endpoint_by_id = self
            .endpoints_for_span_ids(&wanted, &query.time_range)
            .await?;

        // Proxy edges pair a `proxy_in` turn's endpoint with its `proxy_out`
        // sibling's, grouped by pair_id. With trace patching off nothing ever
        // carries a role, so this is empty and the graph shows only entry
        // edges — the documented degradation.
        let mut by_pair_in: HashMap<&str, Vec<(String, u16)>> = HashMap::new();
        let mut by_pair_out: HashMap<&str, Vec<(String, u16)>> = HashMap::new();
        for t in &turns {
            let (Some(pair), Some(role)) = (t.pair_id.as_deref(), t.proxy_role.as_deref()) else {
                continue;
            };
            let Some((ip, port, _)) = endpoint_by_id.get(&t.first_span_id) else {
                continue;
            };
            match role {
                "proxy_in" => by_pair_in
                    .entry(pair)
                    .or_default()
                    .push((ip.clone(), *port)),
                "proxy_out" => by_pair_out
                    .entry(pair)
                    .or_default()
                    .push((ip.clone(), *port)),
                _ => {}
            }
        }
        let mut proxy_counts: HashMap<(String, u16, String, u16), u64> = HashMap::new();
        for (pair, ins) in &by_pair_in {
            let Some(outs) = by_pair_out.get(pair) else {
                continue;
            };
            for (fi, fp) in ins {
                for (ti, tp) in outs {
                    // Same endpoint on both sides is duplicate capture across
                    // interfaces, not a hop.
                    if fi == ti && fp == tp {
                        continue;
                    }
                    *proxy_counts
                        .entry((fi.clone(), *fp, ti.clone(), *tp))
                        .or_insert(0) += 1;
                }
            }
        }
        let proxy_edges: Vec<TopologyEdge> = proxy_counts
            .into_iter()
            .map(
                |((from_ip, from_port, to_ip, to_port), turn_count)| TopologyEdge {
                    from_ip,
                    from_port,
                    to_ip,
                    to_port,
                    turn_count,
                    kind: "proxy".into(),
                },
            )
            .collect();

        // Inbound entry edges. A `proxy_out` turn's inbound side is the hop
        // already covered above.
        let mut entry_counts: HashMap<(String, String, u16), u64> = HashMap::new();
        for t in &turns {
            if t.proxy_role.as_deref() == Some("proxy_out") {
                continue;
            }
            if let Some((to_ip, to_port, caller_ip)) = endpoint_by_id.get(&t.first_span_id) {
                *entry_counts
                    .entry((caller_ip.clone(), to_ip.clone(), *to_port))
                    .or_insert(0) += 1;
            }
        }

        let mut nodes = nodes;
        let edges = assemble_edges(&nodes, entry_counts, proxy_edges);

        // The synthetic `__clients__` super-node, carrying the total of every
        // client edge. It is not an endpoint, so it cannot come out of the
        // endpoint aggregation — both SQL backends append it here, and without
        // it the graph has edges pointing at a node the renderer never
        // received.
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

    async fn topology_turns(&self, range: &TimeRange) -> Result<Vec<TurnInfo>> {
        let s = Search::new(&self.ix.traces, ST_TRACE);
        let limit = MAX_TOPOLOGY_TURNS + 1;
        // `first_span_id` is precomputed at write time, so this never has to
        // pull back a span_ids list that can run to tens of KiB per turn.
        let spl_q = format!(
            "{} | head {limit} | table first_span_id, proxy_role, proxy_pair_id",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(range.start_us),
                &spl::epoch_secs(range.end_us),
            )
            .await?
            .rows();

        let truncated = rows.len() > MAX_TOPOLOGY_TURNS;
        if truncated {
            tracing::warn!(
                target: "aglake::topology",
                cap = MAX_TOPOLOGY_TURNS,
                "aglake: more turns in this window than the topology graph will \
                 consider; the returned graph is truncated. Narrow the time range \
                 for a complete picture."
            );
        }
        Ok(rows
            .into_iter()
            .take(MAX_TOPOLOGY_TURNS)
            .filter_map(|r| {
                let first = r.get("first_span_id").and_then(|v| v.as_str())?;
                if first.is_empty() {
                    return None;
                }
                Some(TurnInfo {
                    first_span_id: first.to_string(),
                    proxy_role: r
                        .get("proxy_role")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    pair_id: r
                        .get("proxy_pair_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
            })
            .collect())
    }

    /// `span_id -> (server_ip, server_port, client_ip)` for the turns' first
    /// calls.
    async fn endpoints_for_span_ids(
        &self,
        ids: &[String],
        range: &TimeRange,
    ) -> Result<HashMap<String, (String, u16, String)>> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(spl::ID_CHUNK) {
            let mut s = Search::new(&self.ix.spans, ST_SPAN);
            s.any_of("id", chunk);
            let spl_q = format!(
                "{} | head {} | table id, server_ip, server_port, client_ip",
                s.build(),
                chunk.len()
            );
            let rows = self
                .search
                .search(
                    &spl_q,
                    &spl::epoch_secs(range.start_us),
                    &spl::epoch_secs(range.end_us),
                )
                .await?
                .rows();
            for r in rows {
                out.insert(
                    string(&r, "id"),
                    (
                        string(&r, "server_ip"),
                        num(&r, "server_port").unwrap_or(0.0) as u16,
                        string(&r, "client_ip"),
                    ),
                );
            }
        }
        Ok(out)
    }
}

struct TurnInfo {
    first_span_id: String,
    proxy_role: Option<String>,
    pair_id: Option<String>,
}

/// Resolve callers to services and fold the entry counts into edges. Pure
/// bookkeeping over data already fetched, and a direct port of the SQL
/// backends' Rust-side half so the graphs agree.
fn assemble_edges(
    nodes: &[TopologyNode],
    entry_counts: HashMap<(String, String, u16), u64>,
    proxy_edges: Vec<TopologyEdge>,
) -> Vec<TopologyEdge> {
    let mut services_by_ip: HashMap<&str, Vec<&TopologyNode>> = HashMap::new();
    for n in nodes {
        services_by_ip
            .entry(n.server_ip.as_str())
            .or_default()
            .push(n);
    }
    let app_of: HashMap<(&str, u16), Option<&str>> = nodes
        .iter()
        .map(|n| ((n.server_ip.as_str(), n.server_port), n.app.as_deref()))
        .collect();
    let is_proxy_app =
        |app: Option<&str>| matches!(app, Some("litellm") | Some("haproxy") | Some("nginx"));

    let resolve_caller = |caller_ip: &str, to_ip: &str, to_port: u16| -> Option<(String, u16)> {
        // When the target is itself a proxy, its inbound traffic is real
        // clients rather than another local service forwarding.
        if is_proxy_app(app_of.get(&(to_ip, to_port)).copied().flatten()) {
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
        if let Some(n) = usable.iter().find(|n| n.app.as_deref() == Some("litellm")) {
            return Some((n.server_ip.clone(), n.server_port));
        }
        if let Some(n) = usable.iter().find(|n| is_proxy_app(n.app.as_deref())) {
            return Some((n.server_ip.clone(), n.server_port));
        }
        let n = usable
            .iter()
            .max_by_key(|n| n.call_count)
            .expect("usable is non-empty");
        Some((n.server_ip.clone(), n.server_port))
    };

    let proxy_pair_set: HashSet<(String, u16, String, u16)> = proxy_edges
        .iter()
        .map(|e| (e.from_ip.clone(), e.from_port, e.to_ip.clone(), e.to_port))
        .collect();

    let mut inferred: HashMap<(String, u16, String, u16), u64> = HashMap::new();
    let mut clients: HashMap<(String, u16), u64> = HashMap::new();
    for ((caller_ip, to_ip, to_port), turn_count) in entry_counts {
        match resolve_caller(&caller_ip, &to_ip, to_port) {
            Some((from_ip, from_port)) => {
                // Suppress a hop the pair sweeper already described.
                if proxy_pair_set.contains(&(from_ip.clone(), from_port, to_ip.clone(), to_port)) {
                    continue;
                }
                *inferred
                    .entry((from_ip, from_port, to_ip, to_port))
                    .or_insert(0) += turn_count;
            }
            None => *clients.entry((to_ip, to_port)).or_insert(0) += turn_count,
        }
    }

    let mut edges = proxy_edges;
    edges.extend(
        inferred
            .into_iter()
            .map(
                |((from_ip, from_port, to_ip, to_port), turn_count)| TopologyEdge {
                    from_ip,
                    from_port,
                    to_ip,
                    to_port,
                    turn_count,
                    kind: "inferred".into(),
                },
            ),
    );
    edges.extend(
        clients
            .into_iter()
            .map(|((to_ip, to_port), turn_count)| TopologyEdge {
                from_ip: "__clients__".into(),
                from_port: 0,
                to_ip,
                to_port,
                turn_count,
                kind: "client".into(),
            }),
    );
    edges
}

// ---------------------------------------------------------------------------
// Row accessors
// ---------------------------------------------------------------------------

fn num(r: &Row, key: &str) -> Option<f64> {
    let v = r.get(key)?;
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

fn int(r: &Row, key: &str) -> u64 {
    num(r, key)
        .filter(|n| *n >= 0.0)
        .map(|n| n as u64)
        .unwrap_or(0)
}

fn string(r: &Row, key: &str) -> String {
    r.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default()
}

/// Read a `values()` cell.
///
/// A multivalue field comes back as a JSON array — except when it holds
/// exactly one value, which collapses to a bare scalar. Handling only the
/// array case would silently drop every single-valued endpoint's model list.
fn str_list(r: &Row, key: &str) -> Vec<String> {
    match r.get(key) {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ip: &str, port: u16, app: Option<&str>, calls: u64) -> TopologyNode {
        TopologyNode {
            server_ip: ip.into(),
            server_port: port,
            app: app.map(str::to_string),
            models: vec![],
            call_count: calls,
        }
    }

    /// A caller IP that hosts no known service is an external client, and its
    /// edge has to say so rather than being dropped.
    #[test]
    fn unknown_callers_become_client_edges() {
        let nodes = vec![node("10.0.0.1", 8000, Some("vllm"), 10)];
        let mut entry = HashMap::new();
        entry.insert(("203.0.113.5".into(), "10.0.0.1".into(), 8000u16), 3u64);
        let edges = assemble_edges(&nodes, entry, vec![]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "client");
        assert_eq!(edges[0].from_ip, "__clients__");
        assert_eq!(edges[0].turn_count, 3);
    }

    /// When the caller IP does host a service, the edge is between services.
    #[test]
    fn known_callers_become_inferred_service_edges() {
        let nodes = vec![
            node("10.0.0.1", 8000, Some("vllm"), 10),
            node("10.0.0.2", 4000, Some("litellm"), 50),
        ];
        let mut entry = HashMap::new();
        entry.insert(("10.0.0.2".into(), "10.0.0.1".into(), 8000u16), 7u64);
        let edges = assemble_edges(&nodes, entry, vec![]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "inferred");
        assert_eq!(edges[0].from_ip, "10.0.0.2");
        assert_eq!(edges[0].from_port, 4000);
        assert_eq!(edges[0].turn_count, 7);
    }

    /// Traffic *into* a proxy is real client traffic — resolving it to another
    /// service on the caller's IP would invent a hop that does not exist.
    #[test]
    fn inbound_traffic_to_a_proxy_stays_a_client_edge() {
        let nodes = vec![
            node("10.0.0.2", 4000, Some("litellm"), 50),
            node("10.0.0.3", 9000, Some("vllm"), 5),
        ];
        let mut entry = HashMap::new();
        entry.insert(("10.0.0.3".into(), "10.0.0.2".into(), 4000u16), 2u64);
        let edges = assemble_edges(&nodes, entry, vec![]);
        assert_eq!(edges[0].kind, "client");
    }

    /// A hop the pair sweeper already described must not also appear as an
    /// inferred edge — it would double the count on that link.
    #[test]
    fn inferred_edges_defer_to_proxy_edges() {
        let nodes = vec![
            node("10.0.0.1", 8000, Some("vllm"), 10),
            node("10.0.0.2", 4000, None, 50),
        ];
        let proxy = vec![TopologyEdge {
            from_ip: "10.0.0.2".into(),
            from_port: 4000,
            to_ip: "10.0.0.1".into(),
            to_port: 8000,
            turn_count: 4,
            kind: "proxy".into(),
        }];
        let mut entry = HashMap::new();
        entry.insert(("10.0.0.2".into(), "10.0.0.1".into(), 8000u16), 7u64);
        let edges = assemble_edges(&nodes, entry, proxy);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].kind, "proxy");
    }

    /// The single-value collapse: a `values()` cell with one entry arrives as
    /// a bare string, and reading only the array case would lose it.
    #[test]
    fn value_lists_read_from_arrays_and_bare_scalars() {
        let r: Row = [
            ("many".to_string(), serde_json::json!(["a", "b"])),
            ("one".to_string(), serde_json::json!("solo")),
            ("num".to_string(), serde_json::json!(5)),
        ]
        .into_iter()
        .collect();
        assert_eq!(str_list(&r, "many"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(str_list(&r, "one"), vec!["solo".to_string()]);
        assert!(str_list(&r, "num").is_empty());
        assert!(str_list(&r, "missing").is_empty());
    }
}
