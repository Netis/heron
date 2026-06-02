#!/usr/bin/env python3
"""Unit tests for tara_invariants — dependency-free (stdlib only), so CI can
run it without installing pytest.

    python3 scripts/staging/tests/test_tara_invariants.py

The HEALTHY fixture mirrors a real soak of keepalive_2sse_pipelined.pcap
(captured 2026-06-02 on heron-stage): 534 pkts, 2 exchanges, 2 LLM calls,
1 turn. Each test perturbs one field and asserts the matching invariant trips.
"""
import json
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import tara_invariants as T  # noqa: E402

HEALTHY = {
    "pkts_received": 534, "pkts_routed": 534, "pkts_parsed": 534, "pkts_truncated": 0,
    "read_errors": 0, "pkts_dropped_malformed": 0, "http_reqs_parsed": 2,
    "http_resps_parsed": 2, "http_exchanges_joined": 2, "http_exchanges_unpaired": 0,
    "wires_detected": 2, "generic_session_id_synth_failed": 0, "calls_ingested": 2,
    "turns_completed": 1, "turns_closed_grace": 0, "turns_closed_idle": 0,
    "calls_dropped_late": 0,
}


def ev(m, fatals=(), ingest=534, min_reqs=1, min_turns=0):
    return T.evaluate(m, list(fatals), ingest, min_reqs=min_reqs, min_turns=min_turns)


def failed(invs):
    return [i["name"] for i in invs if not i["ok"]]


def test_healthy_passes():
    assert failed(ev(HEALTHY)) == [], failed(ev(HEALTHY))


def test_malformed_fails():
    assert "no_malformed" in failed(ev(dict(HEALTHY, pkts_dropped_malformed=3)))


def test_unpaired_exchange_fails():
    assert "exchanges_paired" in failed(ev(dict(HEALTHY, http_exchanges_unpaired=1)))


def test_partial_parse_fails():
    # routed but not all parsed → a parser regression
    assert "all_routed_parsed" in failed(ev(dict(HEALTHY, pkts_parsed=500)))


def test_no_llm_detected_fails():
    assert "llm_detected" in failed(ev(dict(HEALTHY, wires_detected=0)))


def test_sid_synth_failure_fails():
    assert "llm_detected" in failed(ev(dict(HEALTHY, generic_session_id_synth_failed=2)))


def test_no_calls_ingested_fails():
    assert "calls_ingested" in failed(ev(dict(HEALTHY, calls_ingested=0)))


def test_late_drop_fails():
    assert "no_late_drops" in failed(ev(dict(HEALTHY, calls_dropped_late=4)))


def test_read_error_fails():
    assert "capture_clean" in failed(ev(dict(HEALTHY, read_errors=1)))


def test_fatal_log_fails():
    assert "no_fatal_logs" in failed(ev(HEALTHY, fatals=["thread 'x' panicked at foo.rs:1"]))


def test_ingest_cut_short_fails():
    # no "finished reading" line → ingest never completed (early crash)
    assert "ingest_finished" in failed(ev(HEALTHY, ingest=None))


def test_min_reqs_floor_trips_http_seen():
    assert "http_seen" in failed(ev(HEALTHY, min_reqs=9999))


def test_min_turns_floor_trips_turns_built():
    assert "turns_built" in failed(ev(HEALTHY, min_turns=99))


def test_flatten_from_api_shape():
    api = {"data": {"pipelines": [{"metrics": [
        {"name": "pkts_received", "group": "capture", "kind": "counter", "value": 7}]}]}}
    assert T.flatten(api)["pkts_received"] == 7


def test_scan_log_detects_patterns():
    with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as fh:
        fh.write("info ok\npcap-file: finished reading 534 packets\n"
                 "ERROR broken index while merging\n")
        path = fh.name
    try:
        fatals, ingest = T.scan_log(path)
        assert ingest == 534
        assert any("broken index" in f for f in fatals)
    finally:
        os.unlink(path)


def test_cli_exits_nonzero_on_failure():
    payload = {"data": {"pipelines": [{"metrics": [
        {"name": k, "value": v}
        for k, v in dict(HEALTHY, http_exchanges_unpaired=5).items()]}]}}
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as fh:
        json.dump(payload, fh)
        mf = fh.name
    try:
        checker = os.path.join(os.path.dirname(T.__file__), "tara_invariants.py")
        r = subprocess.run([sys.executable, checker, "--metrics-file", mf],
                           capture_output=True)
        assert r.returncode == 1, r.returncode
    finally:
        os.unlink(mf)


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
