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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default="run")
    ap.add_argument("--logfile", default="")
    ap.add_argument("--min-reqs", type=int, default=1)
    ap.add_argument("--min-turns", type=int, default=1)
    ap.add_argument("--metrics-file", default="",
                    help="read metrics JSON from a file instead of stdin")
    args = ap.parse_args()

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
