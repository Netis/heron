#!/usr/bin/env python3
"""tara invariant checker — the judgement half of the soak runner.

Reads a `/api/internal-metrics` snapshot (JSON on stdin) plus the heron log
path, applies a fixed set of parse / pairing / turn / persistence invariants
that must hold for any healthy replay of a well-formed LLM-traffic pcap, and
emits a verdict JSON on stdout.

Kept separate from tara.sh on purpose: pure input→verdict with no I/O beyond
reading the log, so it is unit-testable by feeding a captured metrics JSON
(see tests in scripts/staging/tests/).

Exit code mirrors the verdict: 0 = pass, 1 = fail.
"""
import argparse
import json
import re
import sys


# Log lines that mean the run is structurally broken — any match fails the
# soak outright (these are the crash / corruption classes past incidents hit).
FATAL_PATTERNS = [
    r"panicked at",
    r"\bFATAL\b",
    r"exited abnormally",
    r"pcap-file: read error",
    r"JoinHandle polled after completion",  # the #79 supervisor double-poll
    r"broken index",                        # the #50/#52 DuckDB checkpoint class
]

# Confirms the corpus was fully ingested (not cut short by an early crash).
INGEST_DONE = re.compile(r"pcap-file: finished reading (\d+) packets")


def flatten(metrics_json):
    """{(name): value} from the pipelines[].metrics[] shape. Metric names are
    unique across groups for every name this checker asserts on."""
    out = {}
    data = metrics_json.get("data", metrics_json)
    for pipe in data.get("pipelines", []):
        for m in pipe.get("metrics", []):
            out[m["name"]] = m.get("value", 0)
    return out


def scan_log(path):
    fatals = []
    ingest_pkts = None
    if not path:
        return fatals, ingest_pkts
    try:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                for pat in FATAL_PATTERNS:
                    if re.search(pat, line):
                        fatals.append(line.rstrip()[:300])
                        break
                m = INGEST_DONE.search(line)
                if m:
                    ingest_pkts = int(m.group(1))
    except FileNotFoundError:
        pass
    return fatals, ingest_pkts


def evaluate(m, fatals, ingest_pkts, *, min_reqs, min_turns):
    """Return a list of {name, ok, detail} invariant results."""
    g = lambda k: m.get(k, 0)
    turns_built = g("turns_completed") + g("turns_closed_grace") + g("turns_closed_idle")
    inv = [
        ("ingest_finished", ingest_pkts is not None,
         f"pcap-file read {ingest_pkts} pkts" if ingest_pkts is not None
         else "no 'finished reading' line — ingest cut short / crashed"),
        ("no_fatal_logs", len(fatals) == 0,
         "clean" if not fatals else f"{len(fatals)} fatal line(s): {fatals[0]}"),
        ("capture_clean",
         g("read_errors") == 0 and g("pkts_truncated") == 0,
         f"read_errors={g('read_errors')} truncated={g('pkts_truncated')}"),
        ("all_routed_parsed",
         g("pkts_parsed") > 0 and g("pkts_parsed") >= g("pkts_routed"),
         f"parsed={g('pkts_parsed')} routed={g('pkts_routed')}"),
        ("no_malformed",
         g("pkts_dropped_malformed") == 0,
         f"malformed={g('pkts_dropped_malformed')}"),
        ("http_seen",
         g("http_reqs_parsed") >= min_reqs and g("http_resps_parsed") > 0,
         f"reqs={g('http_reqs_parsed')} resps={g('http_resps_parsed')} (min_reqs={min_reqs})"),
        ("exchanges_paired",
         g("http_exchanges_joined") > 0 and g("http_exchanges_unpaired") == 0,
         f"joined={g('http_exchanges_joined')} unpaired={g('http_exchanges_unpaired')}"),
        ("llm_detected",
         g("wires_detected") > 0 and g("generic_session_id_synth_failed") == 0,
         f"wires={g('wires_detected')} sid_synth_failed={g('generic_session_id_synth_failed')}"),
        ("calls_ingested",
         g("calls_ingested") > 0,
         f"calls_ingested={g('calls_ingested')} (LLM calls reached the turn engine)"),
        ("turns_built",
         turns_built >= min_turns,
         f"turns_built={turns_built} (completed={g('turns_completed')} "
         f"grace={g('turns_closed_grace')} idle={g('turns_closed_idle')}, min={min_turns})"),
        ("no_late_drops",
         g("calls_dropped_late") == 0,
         f"calls_dropped_late={g('calls_dropped_late')}"),
    ]
    return [{"name": n, "ok": bool(ok), "detail": d} for n, ok, d in inv]


def flatten_caps(metrics_json):
    """name -> (value, capacity_or_None) from the /api/internal-metrics shape."""
    out = {}
    data = metrics_json.get("data", metrics_json)
    for pipe in data.get("pipelines", []):
        for m in pipe.get("metrics", []):
            out[m["name"]] = (m.get("value", 0), m.get("capacity"))
    return out


# Backpressure drop counters — any non-zero means a stage couldn't keep up.
BACKPRESSURE_DROPS = [
    "batches_dropped_zmq", "flow_heartbeats_dropped",
    "turn_heartbeats_dropped", "metrics_heartbeats_dropped",
]


def evaluate_load(samples, final_m, fatals, *, max_queue_pct, max_rss_growth_pct, min_pkts):
    """Load/soak invariants from a time series of (rss, metrics) samples + the
    final snapshot. Returns (invariants, summary)."""
    g = lambda k: final_m.get(k, 0)

    # Worst queue-depth utilisation seen across the whole window (any gauge that
    # carries a capacity — the q_* channels).
    worst_q = ("", 0.0)
    for s in samples:
        for name, (value, cap) in flatten_caps(s.get("metrics", {})).items():
            if cap:
                pct = 100.0 * value / cap
                if pct > worst_q[1]:
                    worst_q = (name, pct)

    # RSS series (KB). On a short load soak DuckDB's buffer pool + the
    # allocator arenas fill toward their working set for the first half of the
    # window — that's warm-up, not a leak. Drop a 50% warm-up, then measure
    # *sustained* growth across the steady second half as (last-quartile mean)
    # vs (first-quartile mean of the steady window), so neither one-time
    # start-up growth nor a single sample spike reads as a leak — only a
    # genuine upward trend in steady state trips the invariant. (The strict
    # long-run leak gate is the separate longevity soak.)
    rss = [s.get("rss_kb", 0) for s in samples if s.get("rss_kb", 0) > 0]
    steady = rss[len(rss) // 2:] if len(rss) >= 4 else rss
    if len(steady) >= 4:
        q = max(1, len(steady) // 4)
        rss_base = int(round(sum(steady[:q]) / q))
        rss_peak = int(round(sum(steady[-q:]) / q))
    else:
        rss_base = min(steady) if steady else 0
        rss_peak = max(steady) if steady else 0
    rss_growth_pct = (100.0 * (rss_peak - rss_base) / rss_base) if rss_base > 0 else 0.0

    inv = [
        ("no_fatal_logs", len(fatals) == 0,
         "clean" if not fatals else f"{len(fatals)} fatal line(s): {fatals[0]}"),
        ("load_ran",
         g("pkts_received") >= min_pkts,
         f"pkts_received={g('pkts_received')} (min {min_pkts}) — confirms sustained load"),
        ("no_backpressure_drops",
         all(g(k) == 0 for k in BACKPRESSURE_DROPS),
         ", ".join(f"{k}={g(k)}" for k in BACKPRESSURE_DROPS)),
        ("queues_bounded",
         worst_q[1] < max_queue_pct,
         f"worst {worst_q[0] or '-'}={worst_q[1]:.1f}% (limit {max_queue_pct}%)"),
        ("capture_clean",
         g("read_errors") == 0 and g("pkts_truncated") == 0
         and g("pkts_dropped_malformed") == 0,
         f"read_errors={g('read_errors')} truncated={g('pkts_truncated')} "
         f"malformed={g('pkts_dropped_malformed')}"),
        ("storage_no_flush_errors",
         g("flush_errors") == 0,
         f"flush_errors={g('flush_errors')}"),
        ("rss_stable",
         rss_growth_pct < max_rss_growth_pct,
         f"RSS {rss_base}->{rss_peak} KB = +{rss_growth_pct:.1f}% post-warmup "
         f"(limit {max_rss_growth_pct}%)"),
    ]
    invariants = [{"name": n, "ok": bool(ok), "detail": d} for n, ok, d in inv]
    summary = {
        "samples": len(samples),
        "pkts_received": g("pkts_received"),
        "pkts_parsed": g("pkts_parsed"),
        "flushed_calls": g("flushed_calls"),
        "worst_queue": {"metric": worst_q[0], "pct": round(worst_q[1], 1)},
        "rss_kb": {"base": rss_base, "peak": rss_peak, "growth_pct": round(rss_growth_pct, 1)},
    }
    return invariants, summary


def run_load(args):
    samples = []
    if args.samples:
        try:
            with open(args.samples) as fh:
                for line in fh:
                    line = line.strip()
                    if line:
                        try:
                            samples.append(json.loads(line))
                        except json.JSONDecodeError:
                            pass
        except FileNotFoundError:
            pass

    final_m = {}
    if args.metrics_file:
        try:
            final_m = flatten(json.load(open(args.metrics_file)))
        except Exception:  # noqa: BLE001
            final_m = {}
    if not final_m and samples:
        final_m = flatten(samples[-1].get("metrics", {}))

    fatals, _ = scan_log(args.logfile)
    invariants, summary = evaluate_load(
        samples, final_m, fatals,
        max_queue_pct=args.max_queue_pct,
        max_rss_growth_pct=args.max_rss_growth_pct,
        min_pkts=args.min_pkts,
    )
    if not samples:
        invariants.append({"name": "has_samples", "ok": False,
                           "detail": "no load samples collected"})
    passed = all(i["ok"] for i in invariants)
    verdict = {
        "label": args.label,
        "pass": passed,
        "mode": "load",
        "invariants": invariants,
        "failed": [i["name"] for i in invariants if not i["ok"]],
        "load_summary": summary,
        "fatal_lines": fatals,
    }
    print(json.dumps(verdict, indent=2))
    return 0 if passed else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default="run")
    ap.add_argument("--logfile", default="")
    ap.add_argument("--min-reqs", type=int, default=1)
    ap.add_argument("--min-turns", type=int, default=1)
    ap.add_argument("--metrics-file", default="",
                    help="read metrics JSON from a file instead of stdin")
    # Load/soak mode (--load): assert perf+reliability invariants from a sample
    # series instead of single-pass correctness.
    ap.add_argument("--load", action="store_true")
    ap.add_argument("--samples", default="", help="JSONL of {ts,rss_kb,metrics} samples")
    ap.add_argument("--max-queue-pct", type=float, default=80.0)
    ap.add_argument("--max-rss-growth-pct", type=float, default=50.0)
    ap.add_argument("--min-pkts", type=int, default=10000)
    args = ap.parse_args()

    if args.load:
        return run_load(args)

    raw = open(args.metrics_file).read() if args.metrics_file else sys.stdin.read()
    try:
        metrics_json = json.loads(raw)
    except json.JSONDecodeError as e:
        print(json.dumps({"label": args.label, "pass": False,
                          "error": f"metrics JSON unparseable: {e}"}))
        return 1

    m = flatten(metrics_json)
    fatals, ingest_pkts = scan_log(args.logfile)
    invariants = evaluate(m, fatals, ingest_pkts,
                          min_reqs=args.min_reqs, min_turns=args.min_turns)
    passed = all(i["ok"] for i in invariants)
    verdict = {
        "label": args.label,
        "pass": passed,
        "invariants": invariants,
        "failed": [i["name"] for i in invariants if not i["ok"]],
        "metrics_summary": {
            "pkts_received": m.get("pkts_received", 0),
            "pkts_parsed": m.get("pkts_parsed", 0),
            "http_reqs_parsed": m.get("http_reqs_parsed", 0),
            "http_resps_parsed": m.get("http_resps_parsed", 0),
            "wires_detected": m.get("wires_detected", 0),
            "calls_ingested": m.get("calls_ingested", 0),
            "turns_built": (m.get("turns_completed", 0)
                            + m.get("turns_closed_grace", 0)
                            + m.get("turns_closed_idle", 0)),
            "classifier_unknown": m.get("classifier_unknown", 0),
        },
        "fatal_lines": fatals,
    }
    print(json.dumps(verdict, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
