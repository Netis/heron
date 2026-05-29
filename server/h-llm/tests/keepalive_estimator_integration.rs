//! End-to-end regression for the fallback token estimator.
//!
//! Replays the `keepalive_2sse_{client,server}.bin` fixtures (a real
//! 27-POST agent session captured from a LiteLLM proxy that omits the wire
//! `usage` field) through `HttpParser → HttpJoiner → LlmProcessor` and
//! asserts that every resulting `LlmCall` has non-zero estimated tokens.
//!
//! Without the estimator wiring, those rows would land in the database as
//! `input_tokens=NULL, output_tokens=NULL` and show as `-/-` in the
//! console. The fixture has been verified via `grep` to contain neither
//! `usage` nor `prompt_tokens` nor `completion_tokens` — every estimate
//! produced here is a true fallback, not a fast path past the wire-usage
//! parser.

use std::net::IpAddr;
use std::sync::Arc;

use bytes::BytesMut;

use h_common::internal_metrics::{Metric, MetricsSystem};
use h_llm::model::LlmEvent;
use h_llm::processor::LlmProcessor;
use h_llm::profile::AgentProfileRegistry;
use h_llm::wire_apis::build_default_wire_api_registry;
use h_protocol::http::{HttpParser, ParseResult};
use h_protocol::joiner::HttpJoiner;
use h_protocol::model::HttpParseEvent;
use h_protocol::net::FlowKey;

const CLIENT_BYTES: &[u8] =
    include_bytes!("../../h-protocol/tests/fixtures/keepalive_2sse_client.bin");
const SERVER_BYTES: &[u8] =
    include_bytes!("../../h-protocol/tests/fixtures/keepalive_2sse_server.bin");

#[test]
fn keepalive_real_bytes_estimator_populates_tokens() {
    let cip: IpAddr = "198.51.100.81".parse().unwrap();
    let sip: IpAddr = "192.0.2.81".parse().unwrap();
    let fk = FlowKey::new(String::new(), cip, 54754, sip, 4210);
    let ca = (cip, 54754);
    let sa = (sip, 4210);

    // Stage 1: drive the HTTP parser with concatenated client + server bytes.
    let mut parser = HttpParser::new();
    let mut client_buf = BytesMut::from(CLIENT_BYTES);
    let mut server_buf = BytesMut::from(SERVER_BYTES);
    let mut http_events: Vec<HttpParseEvent> = Vec::new();
    let result = parser.parse(
        &mut client_buf,
        &mut server_buf,
        &fk,
        ca,
        sa,
        0,
        0,
        0,
        &mut http_events,
    );
    assert!(
        matches!(result, ParseResult::Ok),
        "parser must succeed; got {result:?}"
    );

    // Stage 2: feed those parse events through the joiner to get pairs.
    let mut sys = MetricsSystem::new();
    let joiner_w = sys.register_worker(
        "joiner-test",
        &[
            Metric::HttpJoinerDone,
            Metric::HttpJoinerUnpaired,
            Metric::HttpJoinerExpired,
            Metric::HttpJoinerPending,
            Metric::JoinerHeartbeatsReceived,
        ],
    );
    let _svc = sys.start();
    let mut joiner = HttpJoiner::new(joiner_w);
    let mut joiner_events = Vec::new();
    for ev in http_events {
        joiner_events.extend(joiner.process(ev));
    }

    // Stage 3: run through the LLM processor.
    let mut sys2 = MetricsSystem::new();
    let proc_w = sys2.register_worker(
        "llm-test",
        &[
            Metric::WireDetected,
            Metric::WireIgnored,
            Metric::LlmCallsWithAgent,
            Metric::LlmCallsWithoutAgent,
            Metric::LlmGenericToolIdCanonicalized,
            Metric::LlmGenericSessionIdSynthFailed,
            Metric::LlmHeartbeatsReceived,
            Metric::LlmTokensEstimated,
        ],
    );
    let _svc2 = sys2.start();
    let mut proc = LlmProcessor::new(
        Arc::new(build_default_wire_api_registry()),
        Arc::new(AgentProfileRegistry::new()),
        proc_w,
    );
    let mut completes: Vec<Arc<h_llm::model::LlmCall>> = Vec::new();
    for ev in joiner_events {
        for llm_event in proc.process(ev) {
            if let LlmEvent::Complete { call, .. } = llm_event {
                completes.push(call);
            }
        }
    }

    assert!(
        !completes.is_empty(),
        "fixture must yield at least one LlmCall (got 0)"
    );

    let mut zero_token_calls = 0;
    for (i, call) in completes.iter().enumerate() {
        let it = call.input_tokens.unwrap_or(0);
        let ot = call.output_tokens.unwrap_or(0);
        eprintln!(
            "call#{i}: model={} wire_api={} status={:?} in={it} out={ot}",
            call.model, call.wire_api, call.status_code
        );
        if it == 0 && ot == 0 {
            zero_token_calls += 1;
        }
    }

    assert_eq!(
        zero_token_calls,
        0,
        "expected ALL calls to have non-zero estimated tokens; \
         {zero_token_calls} of {} still showed (in=0,out=0)",
        completes.len()
    );

    // At least one call should report a meaningfully large output (these are
    // SSE responses with multi-line replies; tokenizer floor of 0 would be a
    // dead estimator).
    let max_out = completes
        .iter()
        .map(|c| c.output_tokens.unwrap_or(0))
        .max()
        .unwrap_or(0);
    assert!(
        max_out >= 5,
        "expected at least one call to estimate >=5 output tokens, got max={max_out}"
    );
}
