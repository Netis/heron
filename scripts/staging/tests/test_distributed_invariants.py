#!/usr/bin/env python3
"""Unit tests for distributed_invariants — dependency-free (stdlib only).

    python3 scripts/staging/tests/test_distributed_invariants.py

Covers the DISTRIBUTED fan-in invariants (the central health invariants are
covered by test_tara_invariants, since they reuse tara_invariants.evaluate_load).
Each test perturbs a healthy fleet/metrics fixture and asserts the matching
invariant trips.
"""
import json
import os
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import distributed_invariants as D  # noqa: E402

N = 5
EXPECTED = D.expected_source_ids(N, "probe-")


def observed_healthy():
    return {f"probe-{i}": {"calls": 10, "turns": 2} for i in range(N)}


def failed(invs):
    return [i["name"] for i in invs if not i["ok"]]


def ed(observed=None, final=None):
    return D.evaluate_distributed(
        observed if observed is not None else observed_healthy(),
        EXPECTED,
        final if final is not None else {"batches_dropped_zmq": 0},
    )


# ---- distributed fan-in invariants ----------------------------------------

def test_distributed_healthy_passes():
    assert failed(ed()) == [], failed(ed())


def test_missing_probe_fails():
    obs = observed_healthy()
    del obs["probe-2"]
    assert "all_probes_reported" in failed(ed(obs))


def test_silent_probe_fails():
    obs = observed_healthy()
    obs["probe-1"]["calls"] = 0
    assert "every_probe_has_calls" in failed(ed(obs))


def test_unexpected_source_fails():
    obs = observed_healthy()
    obs["rogue-9"] = {"calls": 3}
    assert "no_unexpected_sources" in failed(ed(obs))


def test_bad_frames_fails():
    assert "no_bad_frames" in failed(ed(final={"batches_dropped_zmq": 4}))


# ---- end-to-end run() over temp files -------------------------------------

def metrics_doc(**overrides):
    base = {
        "pkts_received": 50000, "flush_errors": 0, "batches_dropped_zmq": 0,
        "read_errors": 0, "pkts_truncated": 0, "pkts_dropped_malformed": 0,
        "flow_heartbeats_dropped": 0, "turn_heartbeats_dropped": 0,
        "metrics_heartbeats_dropped": 0,
    }
    base.update(overrides)
    return {"data": {"pipelines": [{"metrics": [
        {"name": k, "value": v, "capacity": (1024 if k.startswith("q_") else None)}
        for k, v in base.items()
    ] + [{"name": "q_calls", "value": 1, "capacity": 1024}]}]}}


def write_json(obj):
    f = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(obj, f)
    f.close()
    return f.name


def write_samples(rss_series, metrics):
    f = tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False)
    for rss in rss_series:
        f.write(json.dumps({"ts": 0, "rss_kb": rss, "metrics": metrics}) + "\n")
    f.close()
    return f.name


def args(**over):
    base = dict(
        label="t", logfile="", metrics_file="", samples="", sources_file="",
        expected_probes=N, source_prefix="probe-",
        max_queue_pct=80.0, max_rss_growth_pct=50.0, min_pkts=100,
    )
    base.update(over)
    return types.SimpleNamespace(**base)


def run_healthy(**over):
    m = metrics_doc(**over.pop("metrics_overrides", {}))
    sources = {"source_ids": over.pop("observed", observed_healthy())}
    a = args(
        metrics_file=write_json(m),
        samples=write_samples([100000, 100000, 100000, 100000], m),
        sources_file=write_json(sources),
        **over,
    )
    return D.run(a)


def test_run_healthy_passes():
    assert run_healthy() == 0


def test_run_missing_probe_fails():
    obs = observed_healthy()
    del obs["probe-3"]
    assert run_healthy(observed=obs) == 1


def test_run_bad_frames_fails():
    assert run_healthy(metrics_overrides={"batches_dropped_zmq": 7}) == 1


def test_run_flush_errors_fails():
    # central-health invariant (reused from tara) still gates the distributed soak
    assert run_healthy(metrics_overrides={"flush_errors": 1}) == 1


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
