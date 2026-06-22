//! Golden regression over the curated pcap corpus.
//!
//! Data-driven: reads `testdata/pcaps/corpus.toml`, replays each committed
//! fixture through the FULL pipeline (capture → protocol → llm → turn), projects
//! the extracted `LlmCall`/`Trace` into a DETERMINISTIC JSON shape (no uuids,
//! no timing fields), and compares it to `testdata/pcaps/golden/<id>.json`.
//!
//! - Missing fixtures (or unsmudged git-LFS pointers) are SKIPPED, so the
//!   workspace still builds + tests green without `git lfs pull`.
//! - `HERON_BLESS_GOLDENS=1` (re)writes the goldens instead of asserting.
//!   Use `just corpus bless`.
//!
//! NOTE: the replay harness here intentionally mirrors the one in
//! `integration.rs`. Deduping both onto a shared `tests/common/` module is a
//! follow-up; kept separate for now to isolate this new file from the existing
//! turn-grouping tests.
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

fn pcaps_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pcaps")
}

/// An unsmudged git-LFS pointer is a tiny text file beginning with the LFS
/// header — treat it (and absent files) as "not present" so the test skips.
fn is_real_pcap(p: &Path) -> bool {
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

fn fixture(file: &str) -> Option<PathBuf> {
    let base = pcaps_root();
    for cand in [base.join("corpus").join(file), base.join(file)] {
        if is_real_pcap(&cand) {
            return Some(cand);
        }
    }
    None
}

// ----------------------------------------------------------------------------
// replay harness (full pipeline, 1/1/1 sharding, collecting calls + turns)
// ----------------------------------------------------------------------------

async fn run_pcap_collecting_calls(
    file: &str,
) -> Option<(Vec<h_turn::Trace>, Vec<Arc<h_llm::model::LlmCall>>)> {
    let path = fixture(file)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.corpus",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );

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
    let (turns_tx, mut turns_rx) = mpsc::channel::<h_turn::Trace>(queue_size);
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

    let calls_collector = tokio::spawn(async move {
        let mut acc: Vec<Arc<h_llm::model::LlmCall>> = Vec::new();
        while let Some(c) = calls_rx.recv().await {
            acc.push(c);
        }
        acc
    });
    let metrics_drain = tokio::spawn(async move { while m_out_rx.recv().await.is_some() {} });

    let mut finalized: Vec<h_turn::Trace> = Vec::new();
    while let Some(turn) = turns_rx.recv().await {
        finalized.push(turn);
    }

    let _ = src_task.await;
    let calls = calls_collector.await.unwrap_or_default();
    let _ = metrics_drain.await;
    Some((finalized, calls))
}

// ----------------------------------------------------------------------------
// deterministic projection (NO uuids / timing / synthetic-id VALUES)
// ----------------------------------------------------------------------------

fn opt_str<T: std::fmt::Display>(v: &Option<T>) -> serde_json::Value {
    match v {
        Some(x) => serde_json::Value::String(x.to_string()),
        None => serde_json::Value::Null,
    }
}

fn sorted_strings(v: &[String]) -> Vec<String> {
    let mut s = v.to_vec();
    s.sort();
    s
}

fn project_turn(t: &h_turn::Trace) -> serde_json::Value {
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
fn response_tool_uses(c: &h_llm::model::LlmCall) -> serde_json::Value {
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

fn project_call(c: &h_llm::model::LlmCall) -> serde_json::Value {
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
fn sort_values(mut v: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    v
}

fn build_golden(
    turns: &[h_turn::Trace],
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

// ----------------------------------------------------------------------------
// the data-driven test
// ----------------------------------------------------------------------------

fn manifest_path() -> PathBuf {
    pcaps_root().join("corpus.toml")
}

fn golden_path(id: &str) -> PathBuf {
    pcaps_root().join("golden").join(format!("{id}.json"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corpus_goldens_match() {
    let bless = std::env::var("HERON_BLESS_GOLDENS").is_ok();

    let manifest_src = std::fs::read_to_string(manifest_path())
        .expect("testdata/pcaps/corpus.toml must exist");
    let manifest: toml::Value = toml::from_str(&manifest_src).expect("corpus.toml parses");
    let fixtures = manifest
        .get("fixture")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();

    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for fx in &fixtures {
        let id = fx.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file = fx
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert!(!id.is_empty() && !file.is_empty(), "fixture needs id + file");

        // `status = "pending"` documents a target matrix cell whose capture
        // hasn't been obtained/scrubbed yet — listed for visibility, skipped.
        let status = fx
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("active");
        if status != "active" {
            eprintln!("skip {id}: status={status} (matrix cell pending capture)");
            skipped += 1;
            continue;
        }

        let Some((turns, calls)) = run_pcap_collecting_calls(&file).await else {
            eprintln!("skip {id}: fixture {file} not present (absent or LFS pointer)");
            skipped += 1;
            continue;
        };
        ran += 1;

        let golden = build_golden(&turns, &calls);
        let got = serde_json::to_string_pretty(&golden).unwrap();
        let gp = golden_path(&id);

        if bless {
            std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
            std::fs::write(&gp, format!("{got}\n")).unwrap();
            eprintln!("blessed {id} -> {}", gp.display());
            continue;
        }

        let want = match std::fs::read_to_string(&gp) {
            Ok(s) => s,
            Err(_) => {
                failures.push(format!(
                    "{id}: missing golden {} (run `just corpus bless`)",
                    gp.display()
                ));
                continue;
            }
        };
        if want.trim_end() != got.trim_end() {
            failures.push(format!(
                "{id}: golden mismatch — extracted output differs from {}.\n\
                 If the parser change is intentional, run `just corpus bless` and review the diff.",
                gp.display()
            ));
        }

        // Belt-and-suspenders: assert the manifest's human-readable contract.
        if let Some(expect) = fx.get("expect") {
            if let Some(tc) = expect.get("turn_count").and_then(|v| v.as_integer()) {
                if turns.len() as i64 != tc {
                    failures.push(format!(
                        "{id}: manifest turn_count={tc} but extracted {}",
                        turns.len()
                    ));
                }
            }
        }
    }

    eprintln!("corpus goldens: ran={ran} skipped={skipped} bless={bless}");
    assert!(
        failures.is_empty(),
        "corpus golden failures:\n{}",
        failures.join("\n")
    );
}
