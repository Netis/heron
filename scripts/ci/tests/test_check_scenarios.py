#!/usr/bin/env python3
"""Self-tests for check_scenarios.py — dependency-free (stdlib only).

    python3 scripts/ci/tests/test_check_scenarios.py
"""
import datetime
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import check_scenarios as S  # noqa: E402

TODAY = datetime.date(2026, 6, 1)


def policy():
    return {
        "modules": {
            "m0": {"tier": 0, "path": "server/m0"},
            "m1": {"tier": 1, "path": "server/m1"},
            "m2": {"tier": 2, "path": "server/m2"},
        }
    }


def policy_m0():
    return {"modules": {"m0": {"tier": 0, "path": "server/m0"}}}


def matrix(scenarios):
    return {"kinds": ["unit", "integration", "e2e", "fault"], "scenarios": scenarios}


def sc(sid, tier=0, priority="P0", module="m0", kinds=("unit", "integration")):
    return {"id": sid, "tier": tier, "priority": priority, "module": module,
            "requirement": "r", "invariant": "i", "verification": list(kinds)}


def tag(sid, kind, rel="server/m0/t.rs", line=1):
    return (sid, kind, rel, line)


def test_ok():
    m = matrix([sc("S1"), sc("S2", tier=1, module="m1", kinds=("e2e",))])
    tags = [tag("S1", "unit"), tag("S1", "integration"), tag("S2", "e2e")]
    errs, warns = S.check(policy(), m, {"waivers": []}, tags, today=TODAY)
    assert errs == [], errs
    assert warns == [], warns


def test_missing_kind_fails():
    m = matrix([sc("S1", kinds=("unit", "integration"))])
    errs, _ = S.check(policy(), m, {"waivers": []}, [tag("S1", "unit")], today=TODAY)
    assert any("no verified test for kind integration" in e for e in errs), errs


def test_orphan_tag_fails():
    m = matrix([sc("S1")])
    tags = [tag("S1", "unit"), tag("S1", "integration"), tag("GHOST", "unit")]
    errs, _ = S.check(policy(), m, {"waivers": []}, tags, today=TODAY)
    assert any("orphan" in e for e in errs), errs


def test_unknown_kind_fails():
    m = matrix([sc("S1")])
    tags = [tag("S1", "unit"), tag("S1", "integration"), tag("S1", "smoke")]
    errs, _ = S.check(policy(), m, {"waivers": []}, tags, today=TODAY)
    assert any("unknown kind" in e for e in errs), errs


def test_p0_unit_only_fails():
    m = matrix([sc("S1", kinds=("unit",))])
    errs, _ = S.check(policy(), m, {"waivers": []}, [tag("S1", "unit")], today=TODAY)
    assert any("behavior-level" in e for e in errs), errs


def test_missing_module_scenario_fails():
    m = matrix([sc("S1")])  # m1 is tier 1 and uncovered
    errs, _ = S.check(policy(), m, {"waivers": []}, [tag("S1", "unit"), tag("S1", "integration")], today=TODAY)
    assert any("m1 has no scenario" in e for e in errs), errs


def test_waiver_excuses_missing_kind():
    m = matrix([sc("S1")])
    w = {"waivers": [{"scenario": "S1", "metric": "scenario", "reason": "r",
                      "owner": "o", "expires": "2026-12-31", "tracking": "#1"}]}
    errs, warns = S.check(policy_m0(), m, w, [tag("S1", "unit")], today=TODAY)
    assert errs == [], errs
    assert any("waived" in x for x in warns), warns


def test_expired_waiver_does_not_excuse():
    m = matrix([sc("S1")])
    w = {"waivers": [{"scenario": "S1", "metric": "scenario", "reason": "r",
                      "owner": "o", "expires": "2026-01-01", "tracking": "#1"}]}
    errs, _ = S.check(policy_m0(), m, w, [tag("S1", "unit")], today=TODAY)
    assert any("no verified test for kind integration" in e for e in errs), errs


def test_tag_regex_accepts_e2e_and_digits():
    m = S.TAG_RE.search("// @scenario PIPELINE-E2E-001 e2e")
    assert m and m.group(2) == "e2e", m


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
