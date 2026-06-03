//! Criterion micro-benchmarks for the protocol hot paths — the per-packet work
//! that runs on every captured frame at line rate. A regression here directly
//! cuts the packets/sec heron can sustain before queues back up (the failure
//! mode the load soak in PR1 guards at the system level; these benches localize
//! it to a function).
//!
//! Three groups, all fed REAL wire bytes from the in-repo fixture so the inputs
//! match production shapes (HTTP keepalive + pipelined SSE):
//!   - `decode`            — `de::decode`: raw frame → ParsedPacket (L2/L3/L4).
//!   - `reassemble_parse`  — `FlowWorker::process`: TCP reassembly + flow
//!                           dispatch + HTTP/SSE parse, the headline hot path.
//!   - `shard_hash`        — `FlowKey::shard_hash`: the dispatcher routing hash
//!                           computed once per packet.
//!
//! Run:    cargo bench -p h-protocol
//! Compile-only (the CI gate — cheap, catches API/throughput-path bitrot):
//!         cargo bench -p h-protocol --no-run
//!
//! Skips gracefully (empty groups) if the fixture is absent, so the bench never
//! fails a checkout that lacks it.

use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use h_protocol::de;
use h_protocol::flow::WorkerInput;
use h_protocol::net::ParsedPacket;
use h_protocol::tcp::FlowWorker;

fn fixture() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/keepalive_2sse_pipelined.pcap");
    p.exists().then_some(p)
}

/// Read every frame's raw bytes (+ the file's link type) once.
fn load_frames() -> Option<(u32, Vec<Vec<u8>>)> {
    let path = fixture()?;
    let mut cap = pcap::Capture::from_file(&path).ok()?;
    let link = cap.get_datalink().0 as u32;
    let mut frames = Vec::new();
    while let Ok(pkt) = cap.next_packet() {
        frames.push(pkt.data.to_vec());
    }
    Some((link, frames))
}

/// FlowWorker::process panics on an unregistered metric, so register exactly
/// the set it touches (kept in sync with tcp.rs's `new_test_worker`).
fn bench_metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let m = sys.register_worker(
        "bench",
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
    let _ = sys.start(); // finalize the registry; handles hold their own Arcs
    m
}

fn decode_all(link: u32, frames: &[Vec<u8>]) -> Vec<ParsedPacket> {
    frames
        .iter()
        .filter_map(|f| de::decode(f, link, 0, "bench".to_string()).ok())
        .collect()
}

fn bench_decode(c: &mut Criterion) {
    let Some((link, frames)) = load_frames() else {
        eprintln!("bench: fixture absent — skipping `decode`");
        return;
    };
    let total_bytes: u64 = frames.iter().map(|f| f.len() as u64).sum();
    let mut g = c.benchmark_group("decode");
    g.throughput(Throughput::Bytes(total_bytes));
    g.bench_function("de_decode_fixture", |b| {
        b.iter(|| {
            for f in &frames {
                let _ = black_box(de::decode(black_box(f), link, 0, "bench".to_string()));
            }
        });
    });
    g.finish();
}

fn bench_reassemble_parse(c: &mut Criterion) {
    let Some((link, frames)) = load_frames() else {
        eprintln!("bench: fixture absent — skipping `reassemble_parse`");
        return;
    };
    let parsed = decode_all(link, &frames);
    let mut g = c.benchmark_group("reassemble_parse");
    g.throughput(Throughput::Elements(parsed.len() as u64));
    // Fresh worker + owned packets per batch (process() consumes WorkerInput and
    // mutates flow state); setup is excluded from the measured routine.
    g.bench_function("flowworker_process_fixture", |b| {
        b.iter_batched(
            || (FlowWorker::new(bench_metrics()), parsed.clone()),
            |(mut worker, pkts)| {
                for p in pkts {
                    black_box(worker.process(WorkerInput::Packet(p)));
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });
    g.finish();
}

fn bench_shard_hash(c: &mut Criterion) {
    let Some((link, frames)) = load_frames() else {
        eprintln!("bench: fixture absent — skipping `shard_hash`");
        return;
    };
    let keys: Vec<_> = decode_all(link, &frames)
        .into_iter()
        .map(|p| p.flow_key)
        .collect();
    let mut g = c.benchmark_group("shard_hash");
    g.throughput(Throughput::Elements(keys.len() as u64));
    g.bench_function("flowkey_shard_hash_fixture", |b| {
        b.iter(|| {
            for k in &keys {
                black_box(k.shard_hash());
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_decode, bench_reassemble_parse, bench_shard_hash);
criterion_main!(benches);
