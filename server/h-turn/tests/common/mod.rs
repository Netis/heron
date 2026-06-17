//! Shared test harness for the corpus-driven pipeline tests.
//!
//! Replays a committed pcap fixture through the FULL pipeline
//! (capture → protocol → llm → turn) and projects the extracted
//! `LlmCall`/`AgentTurn` into a DETERMINISTIC, order-insensitive JSON shape (no
//! uuids, no timing fields). Used by `corpus_golden.rs` (golden compare) and
//! `wire_equivalence.rs` (local-vs-distributed differential). Included via
//! `mod common;` in each test binary — `tests/common/mod.rs` is a shared module,
//! NOT compiled as its own test target.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use h_capture::{CaptureSource, PcapFileSource, RoutingSender};
use h_common::internal_metrics::{Metric, MetricsSystem};
use h_protocol::{spawn_flow_dispatcher, spawn_http_joiner_stage, spawn_protocol_stage};
use h_turn::tracker::TrackerConfig;

// ----------------------------------------------------------------------------
// fixture resolution (corpus/ first, then legacy testdata/pcaps/), LFS-aware
// ----------------------------------------------------------------------------

pub fn pcaps_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pcaps")
}

/// An unsmudged git-LFS pointer is a tiny text file beginning with the LFS
/// header — treat it (and absent files) as "not present" so the test skips.
pub fn is_real_pcap(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if meta.len() < 1024 {
        if let Ok(head) = std::fs::read(p) {
            if head.starts_with(b"version https://git-lfs") {
                return false;
            }
        }
    }
    true
}

pub fn fixture(file: &str) -> Option<PathBuf> {
    let base = pcaps_root();
    for cand in [base.join("corpus").join(file), base.join(file)] {
        if is_real_pcap(&cand) {
            return Some(cand);
        }
    }
    None
}

// ----------------------------------------------------------------------------
// pipeline graph (full pipeline, 1/1/1 sharding) decoupled from the front end
// ----------------------------------------------------------------------------

/// Outputs collected from one full-pipeline run.
pub type PipelineOutput = (Vec<h_turn::AgentTurn>, Vec<Arc<h_llm::model::LlmCall>>);

/// Build the full pipeline (dispatcher → protocol → joiner → llm → turn →
/// metrics, 1/1/1 sharding) and return its capture-ingress `raw_tx` plus a
/// future that drains all outputs once `raw_tx` (and every clone) is dropped.
///
/// This is the shared core: `corpus_golden`/`wire_equivalence` differ only in
/// what they wire to `raw_tx` (a local PcapFileSource vs the distributed
/// probe→central transport). The downstream graph below `raw_tx` is identical —
/// which is precisely the equivalence the differential test asserts.
pub fn build_pipeline() -> (
    mpsc::Sender<h_capture::RawPacket>,
    impl std::future::Future<Output = PipelineOutput>,
) {
    let mut metrics_sys = MetricsSystem::new();

    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel::<h_capture::RawPacket>(queue_size);

    let flow_shards = 1usize;
    let turn_shards = 1usize;
    let metrics_shards = 1usize;
    let mut parsed_txs = Vec::with_capacity(flow_shards);
    let mut parsed_rxs = Vec::with_capacity(flow_shards);
    let mut protocol_event_txs = Vec::with_capacity(flow_shards);
    let mut protocol_event_rxs = Vec::with_capacity(flow_shards);
    let mut joiner_event_txs = Vec::with_capacity(flow_shards);
    let mut joiner_event_rxs = Vec::with_capacity(flow_shards);
    for _ in 0..flow_shards {
        let (ptx, prx) = mpsc::channel::<h_protocol::WorkerInput>(queue_size);
        parsed_txs.push(ptx);
        parsed_rxs.push(prx);
        let (etx, erx) = mpsc::channel::<h_protocol::model::HttpParseEvent>(queue_size);
        protocol_event_txs.push(etx);
        protocol_event_rxs.push(erx);
        let (jtx, jrx) = mpsc::channel::<h_protocol::HttpJoinerEvent>(queue_size);
        joiner_event_txs.push(jtx);
        joiner_event_rxs.push(jrx);
    }

    let mut turn_shard_txs = Vec::with_capacity(turn_shards);
    let mut turn_shard_rxs = Vec::with_capacity(turn_shards);
    for _ in 0..turn_shards {
        let (tx, rx) = mpsc::channel::<h_llm::model::TurnShardInput>(queue_size);
        turn_shard_txs.push(tx);
        turn_shard_rxs.push(rx);
    }

    let mut metrics_shard_txs = Vec::with_capacity(metrics_shards);
    let mut metrics_shard_rxs = Vec::with_capacity(metrics_shards);
    for _ in 0..metrics_shards {
        let (tx, rx) = mpsc::channel::<h_llm::model::LlmEvent>(queue_size);
        metrics_shard_txs.push(tx);
        metrics_shard_rxs.push(rx);
    }

    let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<h_llm::model::LlmCall>>(queue_size);
    let (turns_tx, mut turns_rx) = mpsc::channel::<h_turn::AgentTurn>(queue_size);
    let (m_out_tx, mut m_out_rx) = mpsc::channel::<h_metrics::model::LlmMetricsBatch>(queue_size);

    spawn_flow_dispatcher(raw_rx, parsed_txs, "dispatcher", &mut metrics_sys);
    spawn_protocol_stage(parsed_rxs, protocol_event_txs, &mut metrics_sys);
    spawn_http_joiner_stage(protocol_event_rxs, joiner_event_txs, None, &mut metrics_sys);

    let registry = Arc::new(h_llm::agents::build_default_registry());
    let wire_api_registry = Arc::new(h_llm::wire_apis::build_default_wire_api_registry());
    h_llm::spawn_llm_stage(
        joiner_event_rxs,
        turn_shard_txs,
        metrics_shard_txs,
        calls_tx,
        wire_api_registry.clone(),
        registry,
        &mut metrics_sys,
        h_llm::agent_classifier::ClassifierConfig::default(),
        h_common::config::BodyCapConfig::default(),
    );

    h_turn::spawn_turn_stage(
        TrackerConfig::default(),
        turn_shard_rxs,
        turns_tx,
        &mut metrics_sys,
        None,
    );

    h_metrics::spawn_metrics_stage(metrics_shard_rxs, m_out_tx, &mut metrics_sys);

    let _metrics_svc = metrics_sys.start();

    // The drain future: collectors finish only once `raw_tx` (and clones) drop,
    // cascading channel-close down the whole graph.
    let drain = async move {
        let calls_collector = tokio::spawn(async move {
            let mut acc: Vec<Arc<h_llm::model::LlmCall>> = Vec::new();
            while let Some(c) = calls_rx.recv().await {
                acc.push(c);
            }
            acc
        });
        let metrics_drain = tokio::spawn(async move { while m_out_rx.recv().await.is_some() {} });

        let mut finalized: Vec<h_turn::AgentTurn> = Vec::new();
        while let Some(turn) = turns_rx.recv().await {
            finalized.push(turn);
        }
        let calls = calls_collector.await.unwrap_or_default();
        let _ = metrics_drain.await;
        (finalized, calls)
    };

    (raw_tx, drain)
}

// ----------------------------------------------------------------------------
// local replay (pcap → pipeline) — the corpus_golden front end
// ----------------------------------------------------------------------------

pub async fn run_pcap_collecting_calls(file: &str) -> Option<PipelineOutput> {
    let path = fixture(file)?;

    let mut metrics_sys = MetricsSystem::new();
    let source_metrics = metrics_sys.register_worker(
        "capture.corpus",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );
    let _svc = metrics_sys.start();

    let (raw_tx, drain) = build_pipeline();

    let source = PcapFileSource::new(path, "corpus".to_string(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let src_task = tokio::spawn({
        let tx = raw_tx.clone();
        let cancel = cancel.clone();
        async move {
            let _ = Box::new(source)
                .run(RoutingSender::single(tx), source_metrics, cancel)
                .await;
        }
    });
    drop(raw_tx);

    let out = drain.await;
    let _ = src_task.await;
    Some(out)
}

// ----------------------------------------------------------------------------
// deterministic projection (NO uuids / timing / synthetic-id VALUES)
// ----------------------------------------------------------------------------

pub fn opt_str<T: std::fmt::Display>(v: &Option<T>) -> serde_json::Value {
    match v {
        Some(x) => serde_json::Value::String(x.to_string()),
        None => serde_json::Value::Null,
    }
}

pub fn sorted_strings(v: &[String]) -> Vec<String> {
    let mut s = v.to_vec();
    s.sort();
    s
}

pub fn project_turn(t: &h_turn::AgentTurn) -> serde_json::Value {
    let tool_surfaces: Vec<String> = {
        let mut s: Vec<String> = t.tool_surfaces.iter().map(|x| x.to_string()).collect();
        s.sort();
        s
    };
    serde_json::json!({
        "agent_kind": t.agent_kind,
        "wire_api": t.wire_api,
        "call_count": t.call_count,
        "status": t.status.to_string(),
        "models_used": sorted_strings(&t.models_used),
        "subagents_used": sorted_strings(&t.subagents_used),
        "tool_surfaces": tool_surfaces,
        "tool_call_total": t.tool_call_total,
        "agent_topology": opt_str(&t.agent_topology),
        "final_finish_reason": opt_str(&t.final_finish_reason),
        "total_input_tokens": t.total_input_tokens,
        "total_output_tokens": t.total_output_tokens,
        "total_cache_read_input_tokens": t.total_cache_read_input_tokens,
        "total_cache_creation_input_tokens": t.total_cache_creation_input_tokens,
        // presence only — the actual preview text is scrubbed placeholder
        "user_input_preview_present": t.user_input_preview.is_some(),
        "final_answer_preview_present": t.final_answer_preview.is_some(),
    })
}

/// Tool calls in the RECONSTRUCTED response body, with whether each carried a
/// non-empty parsed input/arguments. This is what guards SSE reconstruction
/// (esp. parallel `tool_use` via index-keyed accumulation — an `input: ""` is
/// the pre-fix symptom). Covers anthropic (`content[].tool_use`) and openai
/// chat (`choices[0].message.tool_calls[]`).
pub fn response_tool_uses(c: &h_llm::model::LlmCall) -> serde_json::Value {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let Some(body) = c.response_body.as_deref() else {
        return serde_json::json!([]);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return serde_json::json!([]);
    };
    if let Some(content) = v.get("content").and_then(|x| x.as_array()) {
        for b in content {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let has_input = matches!(b.get("input"),
                    Some(serde_json::Value::Object(o)) if !o.is_empty());
                out.push(serde_json::json!({"name": name, "has_input": has_input}));
            }
        }
    }
    if let Some(tcs) = v.get("choices").and_then(|c| c.get(0))
        .and_then(|c| c.get("message")).and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tcs {
            let f = tc.get("function");
            let name = f.and_then(|f| f.get("name")).and_then(|n| n.as_str())
                .unwrap_or("").to_string();
            let args = f.and_then(|f| f.get("arguments")).and_then(|a| a.as_str())
                .unwrap_or("");
            let has_input = !args.trim().is_empty() && args.trim() != "{}";
            out.push(serde_json::json!({"name": name, "has_input": has_input}));
        }
    }
    // openai responses: output[].type==function_call {name, arguments}
    if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
        for it in items {
            if it.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = it.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                let has_input = !args.trim().is_empty() && args.trim() != "{}";
                out.push(serde_json::json!({"name": name, "has_input": has_input}));
            }
        }
    }
    out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    serde_json::Value::Array(out)
}

pub fn project_call(c: &h_llm::model::LlmCall) -> serde_json::Value {
    serde_json::json!({
        "wire_api": c.wire_api,
        "model": c.model,
        "is_stream": c.is_stream,
        "status_code": c.status_code,
        "finish_reason": opt_str(&c.finish_reason),
        "input_tokens": c.input_tokens,
        "output_tokens": c.output_tokens,
        "cache_read_input_tokens": c.cache_read_input_tokens,
        "cache_creation_input_tokens": c.cache_creation_input_tokens,
        "is_agent_request": c.is_agent_request,
        "tool_surface": opt_str(&c.tool_surface),
        "tool_call_count": c.tool_call_count,
        "tool_names": sorted_strings(&c.tool_names),
        "agent_topology": opt_str(&c.agent_topology),
        // reconstructed response tool calls + input-presence — guards SSE
        // (parallel) tool_use reconstruction
        "response_tool_uses": response_tool_uses(c),
        // synthetic id VALUE is non-deterministic — assert shape/presence only
        "response_id_present": c.response_id.is_some(),
    })
}

/// Stable string sort over projected values → order-insensitive goldens.
pub fn sort_values(mut v: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    v
}

pub fn build_golden(
    turns: &[h_turn::AgentTurn],
    calls: &[Arc<h_llm::model::LlmCall>],
) -> serde_json::Value {
    let turns_p = sort_values(turns.iter().map(project_turn).collect());
    let calls_p = sort_values(calls.iter().map(|c| project_call(c)).collect());
    serde_json::json!({
        "turn_count": turns.len(),
        "call_count": calls.len(),
        "turns": turns_p,
        "calls": calls_p,
    })
}
