//! End-to-end Phase 0 test: synthesized eBPF-style plaintext chunks all the way
//! to an extracted `LlmCall`.
//!
//! This is the strongest validation that the stream→packet synthesis path is
//! sound: it drives the entire production chain — [`FlowSynthesizer`] →
//! `de::decode` → `FlowWorker` (TCP reassembly + HTTP parse) → `HttpJoiner`
//! (request/response pairing) → `LlmProcessor` (wire-API detection + LLM
//! extraction) — and asserts a real `LlmCall` emerges with the right model,
//! path and token accounting. No kernel, no privileges: runs in CI everywhere.

use std::sync::Arc;

use bytes::Bytes;
use h_capture::{
    BootClock, ConnTuple, EbpfPump, FlowSynthesizer, RawPacket, SslEvent, StreamDir, SynthConfig,
};
use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use h_llm::model::{LlmCall, LlmEvent};
use h_llm::processor::LlmProcessor;
use h_llm::wire_apis::build_default_wire_api_registry;
use h_llm::AgentProfileRegistry;
use h_protocol::de::decode;
use h_protocol::joiner::HttpJoiner;
use h_protocol::tcp::FlowWorker;
use h_protocol::WorkerInput;

fn metrics(role: &str) -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(role, Metric::ALL);
    let _ = sys.start();
    w
}

/// Run synthesized frames through the entire capture→protocol→llm chain and
/// return every emitted `LlmEvent`.
fn drive(frames: Vec<RawPacket>) -> Vec<LlmEvent> {
    let mut worker = FlowWorker::new(metrics("synth-flow"));
    let mut joiner = HttpJoiner::new(metrics("synth-joiner"));
    let mut proc = LlmProcessor::new(
        Arc::new(build_default_wire_api_registry()),
        Arc::new(AgentProfileRegistry::new()),
        metrics("synth-llm"),
    );

    let mut out = Vec::new();
    for p in frames {
        let mut parsed = decode(&p.data, p.link_type, p.timestamp_us, p.source_id.clone())
            .unwrap_or_else(|e| panic!("synthesized frame must decode: {e:?}"));
        // Mirror the production `FlowDispatcher`: carry process attribution from
        // the raw packet onto the parsed one (`decode` sees only wire bytes).
        parsed.process = p.process.clone();
        for parse_ev in worker.process(WorkerInput::Packet(parsed)) {
            for join_ev in joiner.process(parse_ev) {
                out.extend(proc.process(join_ev));
            }
        }
    }
    out
}

fn completed_calls(events: &[LlmEvent]) -> Vec<Arc<LlmCall>> {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Complete { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn synthesized_anthropic_call_extracts_llmcall() {
    let mut s = FlowSynthesizer::new(SynthConfig::default());
    let tuple = ConnTuple {
        client: "203.0.113.4:51000".parse().unwrap(),
        server: "192.0.2.10:443".parse().unwrap(),
    };

    let req_body = r#"{"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    let req = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{req_body}",
        req_body.len()
    );
    let resp_body =
        r#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"output_tokens":5}}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{resp_body}",
        resp_body.len()
    );

    let mut frames = s.open(1, tuple, 1_000);
    frames.extend(s.data(1, StreamDir::ClientToServer, req.as_bytes(), 0, 2_000));
    frames.extend(s.data(1, StreamDir::ServerToClient, resp.as_bytes(), 0, 3_000));
    frames.extend(s.close(1, 4_000));

    let events = drive(frames);
    let calls = completed_calls(&events);
    assert_eq!(calls.len(), 1, "exactly one LlmCall should be extracted");

    let call = &calls[0];
    assert_eq!(call.model, "claude-sonnet-4-6");
    assert_eq!(call.request_path, "/v1/messages");
    assert_eq!(call.status_code, Some(200));
    assert_eq!(call.input_tokens, Some(10));
    assert_eq!(call.output_tokens, Some(5));
    assert_eq!(call.source_id, "ebpf", "carries the synthesizer's source_id");
}

/// The differentiating value of the eBPF path: drive the chain from
/// [`EbpfPump`] (which stamps pid/comm/exe onto the synthesized frames from the
/// `SslEvent`s) and assert the attribution survives all the way to the
/// `LlmCall`. This is the end-to-end proof that process attribution threads
/// `RawPacket` → `ParsedPacket` → `TcpFlow` → `Http{Request,Response}Data` →
/// `LlmCall` intact.
#[test]
fn synthesized_call_via_pump_carries_process_attribution() {
    // Offset-0 clock: ktime µs maps straight to epoch µs. No pid allowlist.
    let mut pump = EbpfPump::new(SynthConfig::default(), BootClock::with_offset_us(0), vec![]);
    let tuple = ConnTuple {
        client: "203.0.113.4:51000".parse().unwrap(),
        server: "192.0.2.10:443".parse().unwrap(),
    };

    let req_body = r#"{"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    let req = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{req_body}",
        req_body.len()
    );
    let resp_body =
        r#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"output_tokens":5}}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{resp_body}",
        resp_body.len()
    );

    let exe = Some("/usr/bin/python3.12".to_string());
    let mut frames = pump.on_event(SslEvent::Connect {
        conn_id: 1,
        pid: 31337,
        comm: "python3".into(),
        exe: exe.clone(),
        tuple: Some(tuple),
        ktime_ns: 1_000_000,
    });
    frames.extend(pump.on_event(SslEvent::Data {
        conn_id: 1,
        pid: 31337,
        comm: "python3".into(),
        exe: exe.clone(),
        dir: StreamDir::ClientToServer,
        data: Bytes::from(req.into_bytes()),
        seq_off: 0,
        ktime_ns: 2_000_000,
    }));
    frames.extend(pump.on_event(SslEvent::Data {
        conn_id: 1,
        pid: 31337,
        comm: "python3".into(),
        exe: exe.clone(),
        dir: StreamDir::ServerToClient,
        data: Bytes::from(resp.into_bytes()),
        seq_off: 0,
        ktime_ns: 3_000_000,
    }));
    frames.extend(pump.on_event(SslEvent::Close {
        conn_id: 1,
        ktime_ns: 4_000_000,
    }));

    let events = drive(frames);
    let calls = completed_calls(&events);
    assert_eq!(calls.len(), 1, "exactly one LlmCall should be extracted");

    let proc = calls[0]
        .process
        .as_ref()
        .expect("process attribution must survive the full chain");
    assert_eq!(proc.pid, 31337);
    assert_eq!(proc.comm, "python3");
    assert_eq!(proc.exe.as_deref(), Some("/usr/bin/python3.12"));
}
