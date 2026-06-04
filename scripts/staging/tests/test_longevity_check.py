#!/usr/bin/env python3
"""Unit tests for longevity_check — dependency-free (stdlib only), so CI runs it
without pytest:

    python3 scripts/staging/tests/test_longevity_check.py

Synthesizes a `samples` time series (the shape longevity-soak.sh writes) and
asserts each endurance invariant trips on the matching perturbation.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import longevity_check as L  # noqa: E402


def sample(ts, rss_kb, db_bytes, calls, pkts, flush_errors=0):
    """One samples.jsonl row in the /api/internal-metrics nested shape."""
    return {
        "ts": ts,
        "rss_kb": rss_kb,
        "db_bytes": db_bytes,
        "metrics": {
            "data": {
                "pipelines": [
                    {
                        "metrics": [
                            {"name": "calls_ingested", "value": calls},
                            {"name": "pkts_received", "value": pkts},
                            {"name": "flush_errors", "value": flush_errors},
                        ]
                    }
                ]
            }
        },
    }


def healthy(n=12, calls0=2000, leak=0, bloat=1.0):
    """n samples at scale: RSS flat (+`leak` KB/step), DB ~linear with calls
    (× `bloat` on the per-call cost at the end)."""
    out = []
    for i in range(n):
        calls = calls0 + i * 500
        # ~2 KB/call baseline; bloat ramps the per-call cost over the run.
        per_call = 2000 * (1 + (bloat - 1) * (i / (n - 1)))
        out.append(sample(1_700_000_000 + i * 30, 500_000 + i * leak,
                          int(calls * per_call), calls, 50_000 + i * 20_000))
    return out


def ev(samples, fatals=(), **kw):
    args = dict(max_rss_growth_pct=30.0, max_bytes_per_call_growth_pct=50.0,
                min_pkts=100_000, warmup_frac=0.25, min_calls_for_db_check=1000)
    args.update(kw)
    return L.evaluate(samples, list(fatals), **args)


def failed(invs):
    return [i["name"] for i in invs if not i["ok"]]


def test_healthy_passes():
    assert failed(ev(healthy())) == [], failed(ev(healthy()))


def test_fatal_log_fails():
    assert "no_fatal_logs" in failed(ev(healthy(), fatals=["thread panicked at lib.rs:1"]))


def test_broken_index_fatal_fails():
    assert "no_fatal_logs" in failed(ev(healthy(), fatals=["ERROR broken index while merging"]))


def test_rss_leak_fails():
    # +40 KB every step over 12 samples → well past the 30% post-warmup limit.
    assert "rss_stable" in failed(ev(healthy(leak=40_000)))


def test_flush_errors_fail():
    s = healthy()
    s[-1]["metrics"]["data"]["pipelines"][0]["metrics"][2]["value"] = 3  # flush_errors
    assert "no_flush_errors" in failed(ev(s))


def test_db_bloat_fails_at_scale():
    # Per-call DB cost triples over the run with calls well past the floor.
    assert "db_growth_sane" in failed(ev(healthy(bloat=3.0)))


def test_db_check_skipped_below_min_calls():
    # Tiny call volume throughout (last sample still < 1000) → the bytes/call
    # ratio is noise → skipped (not failed) even though the DB cost balloons.
    s = [sample(1_700_000_000 + i * 30, 500_000, 100_000 * (i + 1) ** 2, 5 * (i + 1),
                100_000 + i * 20_000) for i in range(12)]
    assert max(_calls(x) for x in s) < 1000  # precondition: stays under the floor
    assert "db_growth_sane" not in failed(ev(s))


def _calls(samp):
    return samp["metrics"]["data"]["pipelines"][0]["metrics"][0]["value"]


def test_low_throughput_fails_load_sustained():
    assert "load_sustained" in failed(ev(healthy(), min_pkts=10**9))


def test_too_few_samples_fails():
    assert "enough_samples" in failed(ev(healthy(n=3)))


def test_rss_skipped_when_no_proc():
    # rss_kb=0 (macOS / no /proc) → rss_stable is skipped, not failed.
    s = [sample(1_700_000_000 + i * 30, 0, 4_000_000 + i, 2000 + i * 500, 100_000 + i * 20_000)
         for i in range(12)]
    assert "rss_stable" not in failed(ev(s))


def main():
    tests = [v for k, v in sorted(globals().items())
             if k.startswith("test_") and callable(v)]
    fails = 0
    for t in tests:
        try:
            t()
            print(f"ok   {t.__name__}")
        except AssertionError as e:
            fails += 1
            print(f"FAIL {t.__name__}: {e}")
        except Exception as e:  # noqa: BLE001
            fails += 1
            print(f"ERR  {t.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(tests) - fails}/{len(tests)} passed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
