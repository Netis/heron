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
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use h_capture::ebpf::redact::Redactor;
use h_capture::testpki::{gen_pki, pick_free_port, write_pem};
use h_capture::{
    CaptureSource, PcapFileSource, ProbeUplink, RawPacket, RoutingSender, ThinProbeSource,
};
use h_common::config::{TlsClientConfig, TlsServerConfig};
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

/// `(id, file)` for every `active` fixture in `corpus.toml` (skips `pending`
/// matrix cells). Shared by the corpus-driven differential tests.
pub fn active_fixtures() -> Vec<(String, String)> {
    let src = std::fs::read_to_string(pcaps_root().join("corpus.toml"))
        .expect("testdata/pcaps/corpus.toml must exist");
    let manifest: toml::Value = toml::from_str(&src).expect("corpus.toml parses");
    manifest
        .get("fixture")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|fx| {
            fx.get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("active")
                == "active"
        })
        .map(|fx| {
            (
                fx.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                fx.get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect()
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
                let name = b
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let has_input = matches!(b.get("input"),
                    Some(serde_json::Value::Object(o)) if !o.is_empty());
                out.push(serde_json::json!({"name": name, "has_input": has_input}));
            }
        }
    }
    if let Some(tcs) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tcs {
            let f = tc.get("function");
            let name = f
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = f
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let has_input = !args.trim().is_empty() && args.trim() != "{}";
            out.push(serde_json::json!({"name": name, "has_input": has_input}));
        }
    }
    // openai responses: output[].type==function_call {name, arguments}
    if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
        for it in items {
            if it.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                let name = it
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
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

/// Project a run's turns + calls into the comparable, order-insensitive shape.
pub fn project(out: &PipelineOutput) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let (turns, calls) = out;
    (
        sort_values(turns.iter().map(project_turn).collect()),
        sort_values(calls.iter().map(|c| project_call(c)).collect()),
    )
}

// ----------------------------------------------------------------------------
// distributed replay (pcap → ProbeUplink → mTLS → ThinProbeSource → pipeline)
// ----------------------------------------------------------------------------

/// The Capture* counters `ThinProbeSource` writes (all must be registered or
/// `counter()` panics).
pub const THIN_PROBE_METRICS: &[Metric] = &[
    Metric::CaptureBatchesReceived,
    Metric::CapturePacketsReceived,
    Metric::CaptureTruncatedPackets,
    Metric::CaptureHeartbeatsEmitted,
    Metric::CaptureZmqBatchesDropped,
    Metric::CaptureReadErrors,
];

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// Replay `file` through the FULL distributed transport into the same pipeline a
/// local run uses, and collect the central-side turns/calls. When `redact` is
/// set, the probe scrubs each packet (`Redactor::with_defaults`) before shipping
/// — modelling edge redaction; on the scrubbed corpus this is transparent to
/// extraction, so the result still matches local.
///
/// Asserts a clean probe disconnect raises **zero** central read errors (the
/// graceful-close contract).
pub async fn run_distributed(file: &str, redact: bool) -> Option<PipelineOutput> {
    let path = fixture(file)?;

    let dir = tempfile::tempdir().unwrap();
    let pki = gen_pki("probe-diff");
    let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
    let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
    let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
    let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
    let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);
    let port = pick_free_port();

    // The central's RoutingSender IS the pipeline's capture-ingress — downstream
    // of `raw_tx`, nothing knows a ThinProbeSource (not a PcapFileSource) fed it.
    let (raw_tx, drain) = build_pipeline();
    let cancel = CancellationToken::new();

    let mut central_metrics_sys = MetricsSystem::new();
    let central_metrics = central_metrics_sys.register_worker("thin-probe", THIN_PROBE_METRICS);
    let central_metrics_probe = central_metrics.clone();
    let _central_svc = central_metrics_sys.start();
    let server_tls = TlsServerConfig {
        cert: server_crt,
        key: server_key,
        client_ca: ca.clone(),
    };
    let central = ThinProbeSource::from_config(format!("127.0.0.1:{port}"), &server_tls, None)
        .expect("central from_config");
    let central_cancel = cancel.clone();
    let central_handle = tokio::spawn(async move {
        let _ = Box::new(central)
            .run(
                RoutingSender::single(raw_tx),
                central_metrics,
                central_cancel,
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client_tls = TlsClientConfig {
        cert: client_crt,
        key: client_key,
        server_ca: ca,
    };
    let uplink = ProbeUplink::from_config(
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        "probe-diff".to_string(),
        &client_tls,
    )
    .expect("uplink from_config")
    .with_batching(64, Duration::from_millis(10));
    let (uplink_tx, uplink_rx) = mpsc::channel::<RawPacket>(4096);
    let uplink_cancel = cancel.clone();
    let uplink_handle = tokio::spawn(async move {
        let _ = uplink.run(uplink_rx, uplink_cancel).await;
    });

    // Feed: the SAME PcapFileSource as the local path, optionally through a
    // redaction relay, into the uplink's channel. `src_side` completes once
    // every packet has been handed off and `uplink_tx` is fully dropped.
    let mut src_metrics_sys = MetricsSystem::new();
    let src_metrics = src_metrics_sys.register_worker(
        "capture.probe",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );
    let _src_svc = src_metrics_sys.start();
    let source = PcapFileSource::new(path, "probe-diff".to_string(), None);
    let src_cancel = CancellationToken::new();
    let src_side = if redact {
        let (relay_tx, mut relay_rx) = mpsc::channel::<RawPacket>(4096);
        let relay = tokio::spawn(async move {
            let redactor = Redactor::with_defaults();
            while let Some(mut pkt) = relay_rx.recv().await {
                let mut data = pkt.data.to_vec();
                redactor.redact(&mut data);
                pkt.data = data.into();
                if uplink_tx.send(pkt).await.is_err() {
                    break;
                }
            }
        });
        let src = tokio::spawn(async move {
            let _ = Box::new(source)
                .run(RoutingSender::single(relay_tx), src_metrics, src_cancel)
                .await;
        });
        tokio::spawn(async move {
            let _ = src.await; // pcap EOF → relay_tx drops
            let _ = relay.await; // relay drains + redacts → uplink_tx drops
        })
    } else {
        tokio::spawn(async move {
            let _ = Box::new(source)
                .run(RoutingSender::single(uplink_tx), src_metrics, src_cancel)
                .await;
        })
    };

    // Strict drain order (every packet must arrive before we collect):
    // 1. source side done → uplink_tx fully dropped.
    let _ = timeout(STEP_TIMEOUT, src_side)
        .await
        .expect("source drained");
    // 2. uplink ships its final batch + returns. The uplink returning means
    //    frames are WRITTEN TO THE SOCKET, not yet read by the central.
    let _ = timeout(STEP_TIMEOUT, uplink_handle)
        .await
        .expect("uplink drained");
    // 3. wait for the central to finish reading + forwarding (poll its received
    //    counter until quiescent) before tearing it down.
    let mut prev = u64::MAX;
    for _ in 0..200 {
        let cur = central_metrics_probe
            .counter(Metric::CapturePacketsReceived)
            .get();
        if cur == prev {
            break;
        }
        prev = cur;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 4. cancel the (drained) central → accept loop exits → raw_tx drops.
    cancel.cancel();
    let _ = timeout(STEP_TIMEOUT, central_handle)
        .await
        .expect("central stopped");
    // 5. raw_rx closed → pipeline cascades → collectors finish.
    let out = timeout(Duration::from_secs(30), drain)
        .await
        .expect("pipeline drained");

    // A clean probe disconnect (graceful TLS close_notify) must not surface as a
    // read error — else every probe restart would inflate the counter.
    assert_eq!(
        central_metrics_probe
            .counter(Metric::CaptureReadErrors)
            .get(),
        0,
        "clean probe disconnect raised a central read error"
    );
    Some(out)
}
