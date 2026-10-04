#!/usr/bin/env python3
"""Self-tests for diff_coverage.py — dependency-free (stdlib only).

    python3 scripts/ci/tests/test_diff_coverage.py
"""
import json
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import diff_coverage as D  # noqa: E402

LCOV = """\
SF:/root/server/h-x/src/a.rs
DA:1,1
DA:2,0
DA:3,1
DA:4,5
end_of_record
SF:/root/server/h-y/src/b.rs
DA:10,1
end_of_record
"""


def policy():
    return {
        "modules": {
            "h-x": {"tier": 0, "path": "server/h-x"},
            "h-y": {"tier": 1, "path": "server/h-y"},
            "h-c": {"tier": 2, "path": "server/h-c"},
        },
        "overrides": [],
        "changed_code": {
            "0": {"line": 90, "region": 85},
            "1": {"line": 90, "region": 85},
            "2": {"line": 85, "region": 75},
        },
    }


def test_parse_lcov():
    got = D.parse_lcov(LCOV)
    assert got["/root/server/h-x/src/a.rs"] == {1: 1, 2: 0, 3: 1, 4: 5}, got


def test_parse_diff_hunks_and_devnull():
    diff = (
        "diff --git a/server/h-x/src/a.rs b/server/h-x/src/a.rs\n"
        "--- a/server/h-x/src/a.rs\n"
        "+++ b/server/h-x/src/a.rs\n"
        "@@ -1,0 +2,2 @@\n+x\n+y\n"
        "@@ -9 +12 @@\n+z\n"
        "diff --git a/gone.rs b/gone.rs\n"
        "--- a/gone.rs\n"
        "+++ /dev/null\n"
        "@@ -1,3 +0,0 @@\n-a\n"
    )
    got = D.parse_diff(diff)
    assert got == {"server/h-x/src/a.rs": {2, 3, 12}}, got


def test_parse_regions():
    report = {"data": [{"files": [{
        "filename": "server/h-x/src/a.rs",
        "segments": [
            [1, 0, 3, True, True, False],    # region, covered
            [2, 0, 0, True, True, False],    # region, uncovered
            [3, 0, 0, True, True, True],     # gap region -> ignored
            [4, 0, 7, True, False, False],   # not a region entry
        ],
    }]}]}
    got = D.parse_regions(report)
    assert got == {"server/h-x/src/a.rs": {1: True, 2: False}}, got


def test_evaluate_counts_only_instrumentable_lines():
    lcov = D.parse_lcov("SF:server/h-x/src/a.rs\nDA:1,1\nDA:2,0\nDA:3,1\nDA:4,5\nend_of_record\n")
    results, unmeasured = D.evaluate({"server/h-x/src/a.rs": {1, 2, 3, 4, 99}}, lcov, {}, policy())
    assert unmeasured == []
    assert results[0]["line"] == {"covered": 3, "total": 4}, results


def test_evaluate_unmeasured_file():
    results, unmeasured = D.evaluate({"server/h-y/src/b.rs": {1}}, {}, {}, policy())
    assert unmeasured == ["server/h-y/src/b.rs"], unmeasured
    assert results[1]["line"]["total"] == 0


def test_verdicts_fail_below_target():
    results = {0: {"line": {"covered": 3, "total": 4}, "region": {"covered": 0, "total": 0}}}
    ok, details = D.verdicts(results, policy())
    assert ok is False
    assert details[0]["pct"] == 75.0 and details[0]["target"] == 90


def test_verdicts_pass_at_target():
    results = {0: {"line": {"covered": 90, "total": 100}, "region": {"covered": 0, "total": 0}}}
    ok, details = D.verdicts(results, policy())
    assert ok is True, details


def test_verdicts_region_gate():
    results = {1: {"line": {"covered": 95, "total": 100},
                   "region": {"covered": 80, "total": 100}}}
    ok, details = D.verdicts(results, policy())
    assert ok is False
    assert any(d["metric"] == "region" and not d["ok"] for d in details), details


def test_changed_lines_from_git():
    with tempfile.TemporaryDirectory() as td:
        def git(*a):
            subprocess.run(["git", *a], cwd=td, check=True, capture_output=True)
        git("init", "-q")
        git("config", "user.email", "t@t")
        git("config", "user.name", "t")
        with open(os.path.join(td, "a.rs"), "w") as fh:
            fh.write("x\n" * 3)
        git("add", "."); git("commit", "-qm", "base")
        base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=td, text=True).strip()
        with open(os.path.join(td, "a.rs"), "w") as fh:
            fh.write("x\n" * 3 + "new1\nnew2\n")
        git("add", "."); git("commit", "-qm", "more")
        changed, sha = D.changed_lines(td, base, ["."])
    assert sha == base, (sha, base)
    assert changed.get("a.rs") == {4, 5}, changed


# ---------------------------------------------------------------------------
# End-to-end CLI: run the script against a throwaway git repo so the whole path
# (git merge-base + diff, lcov parse, tier resolution, threshold verdict) is
# exercised in both directions — not just the failing one.
# ---------------------------------------------------------------------------
def _git(td, *args):
    subprocess.run(["git", *args], cwd=td, check=True, capture_output=True)


def _cli_repo(covered):
    """Temp repo whose second commit adds two lines; returns (td, base, lcov, policy, script)."""
    td = tempfile.mkdtemp()
    _git(td, "init", "-q")
    _git(td, "config", "user.email", "t@t")
    _git(td, "config", "user.name", "t")
    os.makedirs(os.path.join(td, "src"))
    with open(os.path.join(td, "src", "a.rs"), "w") as fh:
        fh.write("x\n")
    _git(td, "add", ".")
    _git(td, "commit", "-qm", "base")
    base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=td, text=True).strip()
    with open(os.path.join(td, "src", "a.rs"), "a") as fh:
        fh.write("new1\nnew2\n")
    _git(td, "add", ".")
    _git(td, "commit", "-qm", "more")

    abs_a = os.path.join(td, "src", "a.rs")
    lcov = os.path.join(td, "lcov.info")
    hits = 1 if covered else 0
    with open(lcov, "w") as fh:
        fh.write(f"SF:{abs_a}\nDA:2,{hits}\nDA:3,{hits}\nend_of_record\n")
    policy = os.path.join(td, "policy.json")
    with open(policy, "w") as fh:
        json.dump(
            {
                "modules": {"x": {"tier": 0, "path": "src"}},
                "overrides": [],
                "changed_code": {"0": {"line": 90}},
            },
            fh,
        )
    script = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "diff_coverage.py")
    return td, base, lcov, policy, script


def test_cli_passes_when_changed_lines_are_covered():
    td, base, lcov, policy, script = _cli_repo(covered=True)
    r = subprocess.run(
        [sys.executable, script, "--lcov", lcov, "--base", base,
         "--paths", "src", "--repo-root", td, "--policy", policy],
        capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stdout + r.stderr


def test_cli_fails_when_changed_lines_are_uncovered():
    td, base, lcov, policy, script = _cli_repo(covered=False)
    r = subprocess.run(
        [sys.executable, script, "--lcov", lcov, "--base", base,
         "--paths", "src", "--repo-root", td, "--policy", policy],
        capture_output=True, text=True,
    )
    assert r.returncode == 1, r.stdout + r.stderr


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
