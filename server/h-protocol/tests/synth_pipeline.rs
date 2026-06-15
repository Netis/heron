//! Integration test: stream→packet synthesis through the real pipeline.
//!
//! Phase 0 of the eBPF capture work. An eBPF `SSL_read`/`SSL_write` uprobe sees
//! plaintext byte chunks per connection per direction — no packet headers.
//! [`h_capture::FlowSynthesizer`] dresses those chunks as Ethernet+IP+TCP
//! frames; this test pushes the synthesized frames through the *production*
//! decode → `FlowWorker` (TCP reassembly + HTTP parse) → `HttpJoiner` path and
//! asserts the reassembler/parser reconstruct the same HTTP requests, responses
//! and SSE events a real capture would yield. It validates the only novel
//! correctness surface (frame synthesis) without any kernel or privileges, so
//! it runs unmodified in CI on every platform.

use h_capture::{ConnTuple, FlowSynthesizer, RawPacket, StreamDir, SynthConfig};
use h_common::internal_metrics::{Metric, MetricsSystem};
use h_protocol::de::decode;
use h_protocol::joiner::{HttpJoiner, HttpJoinerEvent};
use h_protocol::model::HttpParseEvent;
use h_protocol::tcp::FlowWorker;
use h_protocol::WorkerInput;

fn tuple() -> ConnTuple {
    ConnTuple {
        client: "203.0.113.4:51000".parse().unwrap(),
        server: "192.0.2.10:443".parse().unwrap(),
    }
}

fn flow_worker() -> FlowWorker {
    let mut sys = MetricsSystem::new();
    let metrics = sys.register_worker("synth-test", Metric::ALL);
    let _ = sys.start();
    FlowWorker::new(metrics)
}

fn joiner() -> HttpJoiner {
    let mut sys = MetricsSystem::new();
    let metrics = sys.register_worker("synth-joiner", Metric::ALL);
    let _ = sys.start();
    HttpJoiner::new(metrics)
}

/// Decode each synthesized frame and run it through the FlowWorker, collecting
/// every emitted `HttpParseEvent`. Mirrors the production capture→protocol hop.
fn run(worker: &mut FlowWorker, pkts: Vec<RawPacket>) -> Vec<HttpParseEvent> {
    let mut out = Vec::new();
    for p in pkts {
        let parsed = decode(&p.data, p.link_type, p.timestamp_us, p.source_id.clone())
            .unwrap_or_else(|e| panic!("synthesized frame must decode, got {e:?}"));
        out.extend(worker.process(WorkerInput::Packet(parsed)));
    }
    out
}

fn count_reqs(events: &[HttpParseEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count()
}

fn count_resps(events: &[HttpParseEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count()
}

fn http_request(path: &str, host: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn http_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Encode `pieces` as an HTTP/1.1 `text/event-stream` chunked response.
fn sse_response(pieces: &[&str]) -> Vec<u8> {
    let mut out =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
    for piece in pieces {
        out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        out.extend_from_slice(piece.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

#[test]
fn synth_request_response_round_trips_through_reassembler() {
    let mut s = FlowSynthesizer::new(SynthConfig::default());
    let mut w = flow_worker();

    let mut frames = s.open(1, tuple(), 1_000);
    frames.extend(s.data(
        1,
        StreamDir::ClientToServer,
        &http_request("/v1/messages", "api.anthropic.com", r#"{"model":"claude"}"#),
        0,
        2_000,
    ));
    frames.extend(s.data(
        1,
        StreamDir::ServerToClient,
        &http_response(r#"{"id":"msg_1","role":"assistant"}"#),
        0,
        3_000,
    ));
    frames.extend(s.close(1, 4_000));

    let events = run(&mut w, frames);
    assert_eq!(count_reqs(&events), 1, "expected one HttpRequest");
    assert_eq!(count_resps(&events), 1, "expected one HttpResponse");

    // The request line and host must survive synthesis intact.
    let req = events
        .iter()
        .find_map(|e| match e {
            HttpParseEvent::HttpRequest(r) => Some(r),
            _ => None,
        })
        .expect("request present");
    assert_eq!(req.method, "POST");
    assert_eq!(req.uri, "/v1/messages");
    assert_eq!(req.header("host"), Some("api.anthropic.com"));
}

#[test]
fn synth_large_body_split_across_segments_reassembles() {
    // Force multi-segment: a body far larger than the segment size must be
    // sliced into several TCP segments and stitched back together by the
    // reassembler with no byte loss.
    let cfg = SynthConfig {
        segment_size: 512,
        ..Default::default()
    };
    let mut s = FlowSynthesizer::new(cfg);
    let mut w = flow_worker();

    let big = "x".repeat(5000);
    let body = format!(r#"{{"data":"{big}"}}"#);
    let req = http_request("/v1/messages", "api.anthropic.com", &body);

    let mut frames = s.open(1, tuple(), 1_000);
    frames.extend(s.data(1, StreamDir::ClientToServer, &req, 0, 2_000));
    frames.extend(s.data(
        1,
        StreamDir::ServerToClient,
        &http_response(r#"{"ok":true}"#),
        0,
        3_000,
    ));

    // The request alone should span multiple segments (sanity on the harness).
    // Use a throwaway synthesizer so this probe doesn't perturb the live flow.
    let req_segments = FlowSynthesizer::new(SynthConfig {
        segment_size: 512,
        ..Default::default()
    })
    .data(1, StreamDir::ClientToServer, &req, 0, 9_000)
    .len();
    assert!(req_segments > 1, "request should be multi-segment");

    let events = run(&mut w, frames);
    assert_eq!(count_reqs(&events), 1);
    assert_eq!(count_resps(&events), 1);
    let req_event = events
        .iter()
        .find_map(|e| match e {
            HttpParseEvent::HttpRequest(r) => Some(r),
            _ => None,
        })
        .expect("request present");
    assert_eq!(
        req_event.body.len(),
        body.len(),
        "multi-segment body reassembled without loss"
    );
}

#[test]
fn synth_sse_streaming_response_emits_sse_events() {
    let mut s = FlowSynthesizer::new(SynthConfig::default());
    let mut w = flow_worker();

    let mut frames = s.open(1, tuple(), 1_000);
    frames.extend(s.data(
        1,
        StreamDir::ClientToServer,
        &http_request("/v1/messages", "api.anthropic.com", r#"{"stream":true}"#),
        0,
        2_000,
    ));
    // Each SSE event arrives as its own server→client chunk, mimicking a real
    // streamed response delivered over successive SSL_read calls.
    let sse = sse_response(&[
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\ndata: {\"delta\":\"Hello\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    frames.extend(s.data(1, StreamDir::ServerToClient, &sse, 0, 3_000));
    frames.extend(s.close(1, 4_000));

    let events = run(&mut w, frames);
    let sse_count = events
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
        .count();
    assert_eq!(count_reqs(&events), 1, "expected the request");
    assert!(
        sse_count >= 3,
        "expected at least 3 SSE events, got {sse_count}"
    );
}

#[test]
fn synth_midstream_without_handshake_syncs_on_request() {
    // Uprobe attached mid-connection: no SYN observed. The reassembler must
    // still sync on the first HTTP request line (its mid-stream path).
    let cfg = SynthConfig {
        emit_handshake: false,
        ..Default::default()
    };
    let mut s = FlowSynthesizer::new(cfg);
    let mut w = flow_worker();

    let mut frames = s.open(1, tuple(), 1_000);
    assert!(frames.is_empty(), "no handshake emitted");
    frames.extend(s.data(
        1,
        StreamDir::ClientToServer,
        &http_request("/v1/chat/completions", "api.openai.com", r#"{"n":1}"#),
        0,
        2_000,
    ));
    frames.extend(s.data(
        1,
        StreamDir::ServerToClient,
        &http_response(r#"{"choices":[]}"#),
        0,
        3_000,
    ));

    let events = run(&mut w, frames);
    assert_eq!(count_reqs(&events), 1, "mid-stream request must parse");
    assert_eq!(count_resps(&events), 1);
}

#[test]
fn synth_two_keepalive_exchanges_pair_in_joiner() {
    // Two sequential request/response pairs on one connection must join into
    // exactly two HttpJoiner Exchanges — proving synthesized keep-alive framing
    // is reconstructed end to end.
    let mut s = FlowSynthesizer::new(SynthConfig::default());
    let mut w = flow_worker();
    let mut j = joiner();

    // Keep-alive: each direction's second message starts at the cumulative byte
    // offset of the first (the BPF per-conn counter the synthesizer now honors).
    let req1 = http_request("/v1/messages", "api.anthropic.com", r#"{"q":1}"#);
    let req2 = http_request("/v1/messages", "api.anthropic.com", r#"{"q":2}"#);
    let resp1 = http_response(r#"{"a":1}"#);
    let resp2 = http_response(r#"{"a":2}"#);

    let mut frames = s.open(1, tuple(), 1_000);
    frames.extend(s.data(1, StreamDir::ClientToServer, &req1, 0, 2_000));
    frames.extend(s.data(1, StreamDir::ServerToClient, &resp1, 0, 3_000));
    frames.extend(s.data(
        1,
        StreamDir::ClientToServer,
        &req2,
        req1.len() as u64,
        4_000,
    ));
    frames.extend(s.data(
        1,
        StreamDir::ServerToClient,
        &resp2,
        resp1.len() as u64,
        5_000,
    ));
    frames.extend(s.close(1, 6_000));

    let parse_events = run(&mut w, frames);
    let mut exchanges = 0;
    for ev in parse_events {
        for je in j.process(ev) {
            if matches!(je, HttpJoinerEvent::Exchange { .. }) {
                exchanges += 1;
            }
        }
    }
    assert_eq!(exchanges, 2, "two keep-alive exchanges must pair");
}
