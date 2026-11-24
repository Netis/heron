#!/usr/bin/env python3
"""Self-tests for check_coverage_policy.py — dependency-free (stdlib only).

    python3 scripts/ci/tests/test_check_coverage_policy.py
"""
import datetime
import json
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import check_coverage_policy as C  # noqa: E402

TODAY = datetime.date(2026, 6, 1)


def policy():
    return {
        "tiers": {
            "0": {"line": 95, "region": 90, "function": 90},
            "1": {"line": 90, "region": 85, "function": 85},
            "2": {"line": 80, "region": 75, "function": 80},
            "3": {"line": 60, "region": 0, "function": 0},
        },
        "changed_code": {"0": {"line": 90, "region": 85}, "2": {"line": 85}},
        "modules": {
            "h-x": {"tier": 0, "path": "server/h-x"},
            "h-y": {"tier": 1, "path": "server/h-y"},
        },
        "overrides": [{"path": "server/h-x/src/ebpf", "tier": 2}],
        "exclusions": {"h-excl": "not a host test target"},
    }


def report(files):
    return {"data": [{"files": files}]}


def fsum(fn, line, region, func):
    return {
        "filename": fn,
        "summary": {
            "lines": {"covered": line[0], "count": line[1]},
            "regions": {"covered": region[0], "count": region[1]},
            "functions": {"covered": func[0], "count": func[1]},
        },
    }


def test_static_ok():
    errs = C.check_static(policy(), {"waivers": []}, ["h-x", "h-y", "h-excl"], today=TODAY)
    assert errs == [], errs


def test_static_unclassified_crate():
    errs = C.check_static(policy(), {"waivers": []}, ["h-x", "h-new"], today=TODAY)
    assert any("unclassified" in e for e in errs), errs


def test_static_both_classified_and_excluded():
    p = policy()
    p["exclusions"]["h-x"] = "conflict"
    errs = C.check_static(p, {"waivers": []}, ["h-x", "h-y"], today=TODAY)
    assert any("both" in e for e in errs), errs


def test_static_unknown_tier():
    p = policy()
    p["modules"]["h-x"]["tier"] = 9
    errs = C.check_static(p, {"waivers": []}, ["h-x", "h-y"], today=TODAY)
    assert any("unknown tier" in e for e in errs), errs


def test_static_expired_waiver():
    w = {"waivers": [{
        "module": "h-x", "metric": "line", "reason": "r", "owner": "o",
        "expires": "2026-01-01", "tracking": "#1",
    }]}
    errs = C.check_static(policy(), w, ["h-x", "h-y"], today=TODAY)
    assert any("expired" in e for e in errs), errs


def test_static_missing_owner_and_malformed_date():
    w = {"waivers": [{"module": "h-x", "metric": "line", "reason": "r",
                      "expires": "soon", "tracking": "#1"}]}
    errs = C.check_static(policy(), w, ["h-x", "h-y"], today=TODAY)
    assert any("missing owner" in e for e in errs), errs
    assert any("malformed" in e for e in errs), errs


def test_package_and_tier_resolution():
    p = policy()
    assert C.package_for_file("server/h-x/src/a.rs", p) == "h-x"
    assert C.tier_for_file("server/h-x/src/a.rs", p) == 0
    assert C.tier_for_file("server/h-x/src/ebpf/probe.rs", p) == 2
    assert C.tier_for_file("server/h-y/nope.rs", p) == 1
    assert C.package_for_file("elsewhere/foo.rs", p) is None


def test_aggregate_report():
    p = policy()
    r = report([
        fsum("/root/server/h-x/src/a.rs", (90, 100), (80, 100), (9, 10)),
        fsum("/root/server/h-x/src/b.rs", (10, 10), (20, 20), (1, 1)),
        fsum("/root/elsewhere/ignored.rs", (0, 5), (0, 5), (0, 1)),
    ])
    agg = C.aggregate_report(r, "/root", p)
    assert set(agg) == {"h-x"}, agg
    assert agg["h-x"]["line"] == round(100 * 100 / 110, 2), agg
    assert agg["h-x"]["function"] == round(100 * 10 / 11, 2), agg


def _baseline(line=91.0, region=80.0, function=90.0):
    return {"crates": {"h-x": {"line": line, "region": region, "function": function}}}


def test_report_within_slack_passes():
    measured = {"h-x": {"line": 90.91, "region": 80.0, "function": 90.0}}
    errs, _ = C.check_report(policy(), _baseline(), {"waivers": []}, measured, today=TODAY)
    assert errs == [], errs


def test_report_drop_fails():
    measured = {"h-x": {"line": 85.0, "region": 80.0, "function": 90.0}}
    errs, _ = C.check_report(policy(), _baseline(), {"waivers": []}, measured, today=TODAY)
    assert any("dropped" in e for e in errs), errs


def test_enforce_tiers_needs_waiver():
    measured = {"h-x": {"line": 90.0, "region": 92.0, "function": 91.0}}
    errs, _ = C.check_report(policy(), _baseline(line=90.0), {"waivers": []},
                             measured, enforce_tiers=True, today=TODAY)
    assert any("tier 0" in e for e in errs), errs


def test_enforce_tiers_with_waiver_passes():
    measured = {"h-x": {"line": 90.0, "region": 92.0, "function": 91.0}}
    w = {"waivers": [{"module": "h-x", "metric": "line", "reason": "r", "owner": "o",
                      "expires": "2026-12-31", "tracking": "#1"}]}
    errs, _ = C.check_report(policy(), _baseline(line=90.0), w,
                             measured, enforce_tiers=True, today=TODAY)
    assert errs == [], errs


def test_tier_waiver_excuses_all_floor_metrics():
    # A crate-level `metric: "tier"` waiver excuses line/region/function at once.
    measured = {"h-x": {"line": 10.0, "region": 10.0, "function": 10.0}}
    w = {"waivers": [{"module": "h-x", "metric": "tier", "reason": "r", "owner": "o",
                      "expires": "2026-12-31", "tracking": "#1"}]}
    errs, _ = C.check_report(policy(), _baseline(line=1.0, region=1.0, function=1.0), w,
                             measured, enforce_tiers=True, today=TODAY)
    assert errs == [], errs


def test_tier_waiver_does_not_excuse_other_modules():
    measured = {"h-y": {"line": 10.0, "region": 10.0, "function": 10.0}}
    w = {"waivers": [{"module": "h-x", "metric": "tier", "reason": "r", "owner": "o",
                      "expires": "2026-12-31", "tracking": "#1"}]}
    baseline = {"crates": {"h-y": {"line": 1.0, "region": 1.0, "function": 1.0}}}
    errs, _ = C.check_report(policy(), baseline, w,
                             measured, enforce_tiers=True, today=TODAY)
    assert any("tier 1" in e for e in errs), errs


def test_write_baseline_roundtrip():
    p = policy()
    r = report([fsum("/root/server/h-x/src/a.rs", (90, 100), (80, 100), (9, 10))])
    agg = C.aggregate_report(r, "/root", p)
    with tempfile.TemporaryDirectory() as td:
        path = os.path.join(td, "baseline.json")
        with open(path, "w") as fh:
            json.dump(C.make_baseline(agg, "/root"), fh)
        loaded = json.load(open(path))
    assert "crates" in loaded and "h-x" in loaded["crates"], loaded


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
