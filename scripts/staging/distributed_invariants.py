#!/usr/bin/env python3
"""distributed-soak invariant checker — the judgement half of the distributed
capture soak (many `heron-probe`s → one central collector over mTLS).

Reuses tara_invariants' LOAD invariants for the central's own health (queues
bounded, RSS stable, no flush errors, no backpressure drops, sustained load) and
adds DISTRIBUTED-specific invariants:

  - all_probes_reported : every expected probe delivered >=1 call to the central
  - every_probe_has_calls : no expected probe is silent
  - no_unexpected_sources : no call is attributed to a source_id outside the fleet
                            (source_id isolation — the central didn't cross-attribute)
  - no_bad_frames : the central rejected no frames (batches_dropped_zmq == 0) —
                    every probe spoke the protocol/version it should

Kept pure (input→verdict, no I/O beyond reading the log) so it is unit-testable
by feeding captured JSON — see scripts/staging/tests/test_distributed_invariants.py.

Inputs:
  --metrics-file  central /api/internal-metrics snapshot (final)
  --samples       JSONL of {ts,rss_kb,metrics} central samples (RSS/queue trend)
  --logfile       central heron log (fatal-line scan)
  --sources-file  JSON {"source_ids": {"<id>": {"calls": N, "turns": M}, ...}}
                  built by the orchestrator from /api/agent-turns
  --expected-probes N (+ --source-prefix) → expected fleet = {<prefix>0..N-1}

Exit 0 = pass, 1 = fail.
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tara_invariants import evaluate_load, flatten, scan_log  # noqa: E402


def expected_source_ids(n, prefix):
    return [f"{prefix}{i}" for i in range(n)]


def evaluate_distributed(observed, expected, final_m):
    """`observed`: {source_id: {"calls": int, "turns": int}}. `expected`: list of
    source_ids that should be present. Returns invariant result dicts."""
    g = lambda k: final_m.get(k, 0)  # noqa: E731
    obs_ids = set(observed.keys())
    exp_ids = set(expected)
    missing = sorted(exp_ids - obs_ids)
    extra = sorted(obs_ids - exp_ids)
    silent = sorted(s for s in exp_ids if observed.get(s, {}).get("calls", 0) <= 0)

    inv = [
        ("all_probes_reported", len(missing) == 0,
         f"{len(obs_ids & exp_ids)}/{len(exp_ids)} expected probes reported"
         + (f"; missing {missing}" if missing else "")),
        ("every_probe_has_calls", len(silent) == 0,
         "every probe delivered >=1 call" if not silent
         else f"no calls attributed to {silent}"),
        ("no_unexpected_sources", len(extra) == 0,
         "no source_id outside the fleet" if not extra
         else f"unexpected source_ids (cross-attribution?) {extra}"),
        ("no_bad_frames", g("batches_dropped_zmq") == 0,
         f"batches_dropped_zmq={g('batches_dropped_zmq')} "
         f"(central rejected frames — version/protocol skew?)"),
    ]
    return [{"name": n, "ok": bool(ok), "detail": d} for n, ok, d in inv]


def load_samples(path):
    samples = []
    if not path:
        return samples
    try:
        with open(path) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    try:
                        samples.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
    except FileNotFoundError:
        pass
    return samples


def run(args):
    samples = load_samples(args.samples)

    final_m = {}
    if args.metrics_file:
        try:
            final_m = flatten(json.load(open(args.metrics_file)))
        except Exception:  # noqa: BLE001
            final_m = {}
    if not final_m and samples:
        final_m = flatten(samples[-1].get("metrics", {}))

    fatals, _ = scan_log(args.logfile)

    # Central health (reused load invariants).
    load_inv, load_summary = evaluate_load(
        samples, final_m, fatals,
        max_queue_pct=args.max_queue_pct,
        max_rss_growth_pct=args.max_rss_growth_pct,
        min_pkts=args.min_pkts,
    )

    # Distributed fan-in invariants.
    observed = {}
    if args.sources_file:
        try:
            observed = json.load(open(args.sources_file)).get("source_ids", {})
        except Exception:  # noqa: BLE001
            observed = {}
    expected = expected_source_ids(args.expected_probes, args.source_prefix)
    dist_inv = evaluate_distributed(observed, expected, final_m)

    invariants = load_inv + dist_inv
    if not samples:
        invariants.append({"name": "has_samples", "ok": False,
                           "detail": "no central samples collected"})
    passed = all(i["ok"] for i in invariants)
    verdict = {
        "label": args.label,
        "pass": passed,
        "mode": "distributed",
        "invariants": invariants,
        "failed": [i["name"] for i in invariants if not i["ok"]],
        "load_summary": load_summary,
        "fleet": {
            "expected": len(expected),
            "reported": len(set(observed.keys()) & set(expected)),
        },
        "fatal_lines": fatals,
    }
    print(json.dumps(verdict, indent=2))
    return 0 if passed else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default="distributed-soak")
    ap.add_argument("--logfile", default="")
    ap.add_argument("--metrics-file", default="")
    ap.add_argument("--samples", default="", help="JSONL of {ts,rss_kb,metrics}")
    ap.add_argument("--sources-file", default="",
                    help='JSON {"source_ids": {"<id>": {"calls": N}}}')
    ap.add_argument("--expected-probes", type=int, default=0)
    ap.add_argument("--source-prefix", default="probe-")
    ap.add_argument("--max-queue-pct", type=float, default=80.0)
    ap.add_argument("--max-rss-growth-pct", type=float, default=50.0)
    ap.add_argument("--min-pkts", type=int, default=10000)
    args = ap.parse_args()
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
