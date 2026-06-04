#!/usr/bin/env python3
"""longevity_check — the judgement half of the nightly longevity soak.

Reads a `samples.jsonl` time series (one line per sample: ts, rss_kb,
db_bytes, and a flattened `/api/internal-metrics` snapshot) plus the heron
log, and asserts that a multi-hour sustained-load run stays HEALTHY over time —
the regression class behind the 2026-06-02 prod outage (a 102 GB DuckDB whose
checkpoint hit a "broken index" FATAL → SIGSEGV):

  - no_fatal_logs       : no panic / "broken index" / checkpoint FATAL ever.
  - no_flush_errors     : storage never reported a flush error.
  - rss_stable          : post-warmup RSS growth < threshold (memory leak).
  - db_growth_sane      : on-disk DuckDB bytes-PER-stored-call stays bounded —
                          the DB may grow with ingestion, but it must grow
                          ~linearly with data, not super-linearly (the index /
                          checkpoint bloat that ran prod to 102 GB).
  - load_sustained      : the run actually ingested a meaningful packet volume
                          for its whole window (didn't silently stall).

Pure input→verdict (stdlib only, no pytest) so it is unit-testable by feeding a
synthesized series; mirrors tara_invariants.py. Exit 0 = pass, 1 = regression.
"""
import argparse
import json
import re
import sys

FATAL_PATTERNS = [
    r"panicked at",
    r"\bFATAL\b",
    r"broken index",            # the #50/#52 DuckDB checkpoint class (the 102 GB FATAL)
    r"exited abnormally",
    r"JoinHandle polled after completion",
]


def scan_log(path):
    fatals = []
    if not path:
        return fatals
    try:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                for pat in FATAL_PATTERNS:
                    if re.search(pat, line):
                        fatals.append(line.rstrip()[:300])
                        break
    except FileNotFoundError:
        pass
    return fatals


def flatten(metrics_json):
    out = {}
    data = metrics_json.get("data", metrics_json) if isinstance(metrics_json, dict) else {}
    for pipe in data.get("pipelines", []):
        for m in pipe.get("metrics", []):
            out[m["name"]] = m.get("value", 0)
    return out


def load_samples(path):
    rows = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return rows


def _metric(sample, name, default=0):
    m = sample.get("metrics")
    if isinstance(m, dict):
        flat = flatten(m)
        return flat.get(name, default)
    return default


def evaluate(samples, fatals, *, max_rss_growth_pct, max_bytes_per_call_growth_pct,
             min_pkts, warmup_frac, min_calls_for_db_check):
    """Return a list of {name, ok, detail} invariant results."""
    inv = []
    inv.append(("no_fatal_logs", len(fatals) == 0,
                "clean" if not fatals else f"{len(fatals)} fatal line(s): {fatals[0]}"))

    if len(samples) < 4:
        inv.append(("enough_samples", False,
                    f"only {len(samples)} samples — run too short / sampler stalled"))
        return [{"name": n, "ok": bool(ok), "detail": d} for n, ok, d in inv]
    inv.append(("enough_samples", True, f"{len(samples)} samples"))

    warm = max(1, int(len(samples) * warmup_frac))
    post = samples[warm:]

    # --- memory leak: post-warmup RSS growth ---
    rss = [s.get("rss_kb", 0) for s in post if s.get("rss_kb", 0) > 0]
    if len(rss) >= 2 and rss[0] > 0:
        growth = (rss[-1] - rss[0]) / rss[0] * 100.0
        inv.append(("rss_stable", growth <= max_rss_growth_pct,
                    f"RSS {rss[0]}→{rss[-1]} KB = {growth:+.1f}% post-warmup (limit {max_rss_growth_pct}%)"))
    else:
        # RSS sampling is Linux-/proc-only; absent on macOS dry-runs.
        inv.append(("rss_stable", True, "no RSS samples (non-Linux?) — skipped"))

    # --- no flush errors across the whole run ---
    max_flush_err = max((_metric(s, "flush_errors") for s in samples), default=0)
    inv.append(("no_flush_errors", max_flush_err == 0, f"max flush_errors={max_flush_err}"))

    # --- DB growth sanity: bytes per stored call must not balloon ---
    # The DB grows with ingestion; what catches the 102 GB class is *bytes per
    # row* exploding (index/checkpoint bloat) rather than tracking the data.
    def bytes_per_call(s):
        calls = _metric(s, "calls_ingested")
        db = s.get("db_bytes", 0)
        return (db / calls) if calls > 0 and db > 0 else None

    # Per-call DB cost is dominated by fixed schema / page / checkpoint overhead
    # until enough rows land, so the ratio is only meaningful at scale — gate it
    # on a minimum call volume (a few-minute smoke would false-fail otherwise; a
    # real nightly run is far past this floor).
    calls_last = _metric(samples[-1], "calls_ingested")
    early = next((bytes_per_call(s) for s in post if bytes_per_call(s)), None)
    late = next((bytes_per_call(s) for s in reversed(post) if bytes_per_call(s)), None)
    if calls_last < min_calls_for_db_check:
        inv.append(("db_growth_sane", True,
                    f"skipped — only {calls_last} calls (< {min_calls_for_db_check} "
                    "needed for a meaningful bytes/call ratio)"))
    elif early and late:
        growth = (late - early) / early * 100.0
        inv.append(("db_growth_sane", growth <= max_bytes_per_call_growth_pct,
                    f"bytes/call {early:.0f}→{late:.0f} = {growth:+.1f}% "
                    f"(limit {max_bytes_per_call_growth_pct}%)"))
    else:
        inv.append(("db_growth_sane", False,
                    "no db_bytes / calls_ingested samples to size the DB growth"))

    # --- load actually sustained for the window ---
    last_pkts = _metric(samples[-1], "pkts_received")
    inv.append(("load_sustained", last_pkts >= min_pkts,
                f"pkts_received={last_pkts} (min {min_pkts})"))

    return [{"name": n, "ok": bool(ok), "detail": d} for n, ok, d in inv]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--samples", required=True)
    ap.add_argument("--logfile", default="")
    ap.add_argument("--label", default="longevity")
    ap.add_argument("--max-rss-growth-pct", type=float, default=30.0)
    ap.add_argument("--max-bytes-per-call-growth-pct", type=float, default=50.0)
    ap.add_argument("--min-pkts", type=int, default=100000)
    ap.add_argument("--warmup-frac", type=float, default=0.25)
    ap.add_argument("--min-calls-for-db-check", type=int, default=1000)
    args = ap.parse_args()

    samples = load_samples(args.samples)
    fatals = scan_log(args.logfile)
    inv = evaluate(samples, fatals,
                   max_rss_growth_pct=args.max_rss_growth_pct,
                   max_bytes_per_call_growth_pct=args.max_bytes_per_call_growth_pct,
                   min_pkts=args.min_pkts, warmup_frac=args.warmup_frac,
                   min_calls_for_db_check=args.min_calls_for_db_check)
    passed = all(i["ok"] for i in inv)

    def g(s, k):
        return s.get(k, 0) if s else 0
    first, last = (samples[0] if samples else {}), (samples[-1] if samples else {})
    verdict = {
        "label": args.label,
        "pass": passed,
        "invariants": inv,
        "failed": [i["name"] for i in inv if not i["ok"]],
        "summary": {
            "samples": len(samples),
            "rss_kb": {"first": g(first, "rss_kb"), "last": g(last, "rss_kb")},
            "db_bytes": {"first": g(first, "db_bytes"), "last": g(last, "db_bytes")},
            "calls_ingested_last": _metric(last, "calls_ingested") if last else 0,
            "pkts_received_last": _metric(last, "pkts_received") if last else 0,
        },
        "fatal_lines": fatals,
    }
    print(json.dumps(verdict, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
