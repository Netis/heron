//! Integration test: replay real captured bytes from a TLS-cleartext-keepalive
//! HTTP/1.1 flow that the live deployment failed to parse fully.
//!
//! Fixtures `keepalive_2sse_{client,server}.bin` come from `tcpdump -i any
//! tcp port 4210` of an agent session against a LiteLLM proxy. The single
//! TCP flow carries 2 sequential POST `/v1/chat/completions` + 2 SSE chunked
//! `text/event-stream` responses. Both responses are fully terminated with
//! `0\r\n\r\n` (verified by raw byte inspection), so a correct HTTP/1.1
//! keepalive parser must emit exactly 2 HttpRequest and 2 HttpResponse
//! events when fed the concatenated client and server byte streams.

use std::net::IpAddr;
use std::path::Path;

use bytes::BytesMut;

use ts_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use ts_protocol::de::decode;
use ts_protocol::http::{HttpParser, ParseResult};
use ts_protocol::model::HttpParseEvent;
use ts_protocol::net::FlowKey;
use ts_protocol::tcp::{FlowWorker, TcpFlow};
use ts_protocol::WorkerInput;

const CLIENT_BYTES: &[u8] = include_bytes!("fixtures/keepalive_2sse_client.bin");
const SERVER_BYTES: &[u8] = include_bytes!("fixtures/keepalive_2sse_server.bin");

fn flow() -> (FlowKey, (IpAddr, u16), (IpAddr, u16)) {
    let fk = FlowKey::new(
        String::new(),
        "10.40.1.81".parse().unwrap(),
        54754,
        "172.16.103.81".parse().unwrap(),
        4210,
    );
    let ca = ("10.40.1.81".parse().unwrap(), 54754);
    let sa = ("172.16.103.81".parse().unwrap(), 4210);
    (fk, ca, sa)
}

#[test]
fn keepalive_two_sse_chunked_responses_real_bytes_one_shot() {
    let (fk, ca, sa) = flow();
    let mut parser = HttpParser::new();
    let mut client_buf = BytesMut::from(CLIENT_BYTES);
    let mut server_buf = BytesMut::from(SERVER_BYTES);
    let mut output = Vec::new();

    let result = parser.parse(
        &mut client_buf,
        &mut server_buf,
        &fk,
        ca,
        sa,
        0,
        0,
        0,
        &mut output,
    );

    let req_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count();
    let resp_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count();
    let sse_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
        .count();

    eprintln!(
        "parse result = {:?}, events: req={} resp={} sse={} client_buf_remain={} server_buf_remain={}",
        result,
        req_count,
        resp_count,
        sse_count,
        client_buf.len(),
        server_buf.len()
    );

    assert_eq!(
        result,
        ParseResult::Ok,
        "parser must not need resync on a clean keepalive flow"
    );
    assert_eq!(req_count, 2, "expected 2 HttpRequest, got {}", req_count);
    assert_eq!(resp_count, 2, "expected 2 HttpResponse, got {}", resp_count);
    assert!(
        sse_count > 0,
        "expected SSE events to be parsed, got {}",
        sse_count
    );
    assert_eq!(
        client_buf.len(),
        0,
        "client buffer must be fully consumed; {} bytes remain",
        client_buf.len()
    );
    assert_eq!(
        server_buf.len(),
        0,
        "server buffer must be fully consumed; {} bytes remain",
        server_buf.len()
    );
}

/// Same fixture, but feed the bytes to the parser in a realistic interleaved
/// way: alternate small chunks of client and server data so the parser sees
/// the SECOND request appear in client_buf BEFORE the FIRST response is fully
/// drained from server_buf. This mirrors what the live deployment sees on a
/// keepalive connection where the client immediately fires off another POST
/// once the previous SSE [DONE] is received but the SSE response trailer
/// (`0\r\n\r\n`) for the previous request has not yet arrived in our buffer.
#[test]
fn keepalive_two_sse_chunked_responses_real_bytes_interleaved_chunked_feed() {
    let (fk, ca, sa) = flow();
    let mut parser = HttpParser::new();
    let mut client_buf = BytesMut::new();
    let mut server_buf = BytesMut::new();
    let mut output = Vec::new();

    // Reasonable approximation of MTU-bound TCP segments.
    let chunk = 1448usize;
    let mut ci = 0usize;
    let mut si = 0usize;
    let mut ts: i64 = 0;
    while ci < CLIENT_BYTES.len() || si < SERVER_BYTES.len() {
        if ci < CLIENT_BYTES.len() {
            let end = (ci + chunk).min(CLIENT_BYTES.len());
            client_buf.extend_from_slice(&CLIENT_BYTES[ci..end]);
            ci = end;
        }
        if si < SERVER_BYTES.len() {
            let end = (si + chunk).min(SERVER_BYTES.len());
            server_buf.extend_from_slice(&SERVER_BYTES[si..end]);
            si = end;
        }
        ts += 1_000;
        let r = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            ts,
            ts,
            ts,
            &mut output,
        );
        assert_eq!(r, ParseResult::Ok, "parser should not need resync mid-stream");
    }

    // Final drain pass after both sides exhausted.
    let _ = parser.parse(
        &mut client_buf,
        &mut server_buf,
        &fk,
        ca,
        sa,
        ts,
        ts,
        ts,
        &mut output,
    );

    let req_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count();
    let resp_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count();
    eprintln!(
        "interleaved-chunked: req={} resp={} client_remain={} server_remain={}",
        req_count,
        resp_count,
        client_buf.len(),
        server_buf.len()
    );
    assert_eq!(req_count, 2, "expected 2 HttpRequest, got {}", req_count);
    assert_eq!(resp_count, 2, "expected 2 HttpResponse, got {}", resp_count);
}

/// Adversarial variant: feed ALL of client side first, then ALL of server side.
/// On a keepalive flow with proper request/response ordering, the parser must
/// still handle this — both POSTs arrive in client_buf, then both SSE
/// responses arrive in server_buf. The parser must walk through 2 full
/// request/response cycles even though the second request was buffered while
/// the parser was in WaitingForResponse for the first.
#[test]
fn keepalive_client_first_then_server_real_bytes() {
    let (fk, ca, sa) = flow();
    let mut parser = HttpParser::new();
    let mut client_buf = BytesMut::from(CLIENT_BYTES);
    let mut server_buf = BytesMut::new();
    let mut output = Vec::new();

    // Phase 1: only client bytes available.
    let r = parser.parse(
        &mut client_buf,
        &mut server_buf,
        &fk,
        ca,
        sa,
        1000,
        2000,
        2000,
        &mut output,
    );
    assert_eq!(r, ParseResult::Ok);

    // Phase 2: server bytes arrive.
    server_buf.extend_from_slice(SERVER_BYTES);
    let r = parser.parse(
        &mut client_buf,
        &mut server_buf,
        &fk,
        ca,
        sa,
        3000,
        4000,
        4000,
        &mut output,
    );
    assert_eq!(r, ParseResult::Ok);

    let req_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count();
    let resp_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count();
    eprintln!(
        "client-first: req={} resp={} client_remain={} server_remain={}",
        req_count,
        resp_count,
        client_buf.len(),
        server_buf.len()
    );
    assert_eq!(req_count, 2, "expected 2 HttpRequest, got {}", req_count);
    assert_eq!(resp_count, 2, "expected 2 HttpResponse, got {}", resp_count);
}

/// End-to-end TCP-reassembly + HTTP-parse replay through the SAME public API
/// path that production uses (`TcpFlow::push`). Reads the saved pcap one
/// packet at a time, decodes via `ts_protocol::de::decode`, and pushes each
/// `ParsedPacket` into a single `TcpFlow`. Asserts the parser surfaces the
/// 2 POSTs and 2 SSE responses present in the capture.
///
/// This is the canonical TDD repro for the live deployment failure where
/// `http_reqs_parsed` / `http_resps_parsed` < the request/response counts
/// visible in the same pcap.
#[test]
fn keepalive_two_sse_chunked_responses_pcap_replay_through_tcp_flow() {
    let pcap_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/keepalive_2sse_pipelined.pcap");
    let mut cap = pcap::Capture::from_file(&pcap_path)
        .unwrap_or_else(|e| panic!("open {}: {e}", pcap_path.display()));
    let link_type = cap.get_datalink().0 as u32;

    let (fk, _, _) = flow();
    let mut tcp_flow = TcpFlow::new(fk.clone());
    let mut output = Vec::new();
    let mut decoded = 0usize;
    let mut decode_errs = 0usize;

    while let Ok(pkt) = cap.next_packet() {
        let ts_us = pkt.header.ts.tv_sec as i64 * 1_000_000 + pkt.header.ts.tv_usec as i64;
        match decode(pkt.data, link_type, ts_us, String::new()) {
            Ok(parsed) => {
                tcp_flow.push(&parsed, &mut output);
                decoded += 1;
            }
            Err(_) => decode_errs += 1,
        }
    }

    let req_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count();
    let resp_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count();
    let sse_count = output
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
        .count();

    eprintln!(
        "pcap-replay: decoded={} decode_errs={} req={} resp={} sse={}",
        decoded, decode_errs, req_count, resp_count, sse_count
    );

    assert_eq!(
        req_count, 2,
        "expected 2 HttpRequest events, got {}",
        req_count
    );
    assert_eq!(
        resp_count, 2,
        "expected 2 HttpResponse events, got {}",
        resp_count
    );
}

fn flow_worker_with_metrics() -> (FlowWorker, MetricsWorker) {
    let mut sys = MetricsSystem::new();
    let metrics = sys.register_worker(
        "test-flow-worker",
        &[
            Metric::NetPacketsParsed,
            Metric::HttpParseReq,
            Metric::HttpParseResp,
            Metric::SseEventsParsed,
            Metric::HttpResyncEvents,
            Metric::TcpOutOfOrderDrops,
            Metric::TcpOutOfOrderBuffered,
            Metric::TcpRetransmissionsIgnored,
            Metric::FlowsExpired,
            Metric::FlowHeartbeatsReceived,
        ],
    );
    let _ = sys.start();
    (FlowWorker::new(metrics.clone()), metrics)
}

/// Single-flow pcap replay through `FlowWorker` — the same code path
/// production uses (FlowWorker dispatching by flow_key hash). Asserts the
/// worker emits exactly the 2 requests + 2 responses present in the
/// `keepalive_2sse_pipelined.pcap` fixture.
#[test]
fn keepalive_pcap_replay_through_flow_worker() {
    let pcap_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/keepalive_2sse_pipelined.pcap");
    let mut cap = pcap::Capture::from_file(&pcap_path)
        .unwrap_or_else(|e| panic!("open {}: {e}", pcap_path.display()));
    let link_type = cap.get_datalink().0 as u32;

    let (mut worker, _metrics) = flow_worker_with_metrics();
    let mut all_events = Vec::new();

    while let Ok(pkt) = cap.next_packet() {
        let ts_us = pkt.header.ts.tv_sec as i64 * 1_000_000 + pkt.header.ts.tv_usec as i64;
        if let Ok(parsed) = decode(pkt.data, link_type, ts_us, String::new()) {
            all_events.extend(worker.process(WorkerInput::Packet(parsed)));
        }
    }

    let req_count = all_events
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
        .count();
    let resp_count = all_events
        .iter()
        .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
        .count();
    assert_eq!(req_count, 2, "expected 2 HttpRequest, got {}", req_count);
    assert_eq!(resp_count, 2, "expected 2 HttpResponse, got {}", resp_count);
}
