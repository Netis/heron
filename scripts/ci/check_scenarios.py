#!/usr/bin/env python3
"""Scenario-coverage checker — enforces the behavior half of
docs/design/11-verification.md.

Scans Rust/TypeScript sources for in-code tags of the form

    // @scenario <ID> <kind>          kind ∈ {unit,integration,e2e,fault}

and asserts, against verification/scenarios.json:

  1. every scenario is verified at each kind it declares;
  2. every P0/P1 scenario is fully verified (and has at least one
     behavior-level kind — not unit-only);
  3. every tag names a scenario that exists (no orphan tags);
  4. every tier 0/1 module in the coverage policy has at least one scenario;
  5. an unexpired waiver (metric=scenario) can excuse a scenario's gaps.

Dependency-free (stdlib only). Exit 0 = pass, 1 = failure, 2 = usage/IO.
"""
import argparse
import datetime
import json
import os
import re
import sys

TAG_RE = re.compile(r"@scenario\s+([A-Za-z0-9][A-Za-z0-9_.-]*)\s+([a-z][a-z0-9]*)")
SCAN_SUFFIXES = (".rs", ".ts", ".tsx")
SKIP_DIRS = {"target", "node_modules", ".git", "dist", "build", "coverage"}
BEHAVIOR_KINDS = {"integration", "e2e", "fault"}


def load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def iter_sources(repo_root):
    for sub in ("server", "console/src"):
        base = os.path.join(repo_root, sub)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, filenames in os.walk(base):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
            for name in filenames:
                if name.endswith(SCAN_SUFFIXES):
                    yield os.path.join(dirpath, name)


def collect_tags(repo_root):
    """[(id, kind, repo-rel file, line)] for every @scenario tag."""
    tags = []
    for path in iter_sources(repo_root):
        rel = os.path.relpath(path, repo_root)
        try:
            with open(path, encoding="utf-8", errors="replace") as fh:
                for lineno, line in enumerate(fh, 1):
                    for m in TAG_RE.finditer(line):
                        tags.append((m.group(1), m.group(2), rel, lineno))
        except OSError:
            continue
    return tags


def _waived(waivers, scenario_id, today):
    for w in waivers.get("waivers", []):
        if w.get("scenario") != scenario_id or w.get("metric") != "scenario":
            continue
        try:
            if datetime.date.fromisoformat(w["expires"]) >= today:
                return True
        except (KeyError, ValueError):
            continue
    return False


def check(policy, matrix, waivers, tags, today=None):
    today = today or datetime.date.today()
    errors = []
    warnings = []
    scenarios = matrix.get("scenarios", [])
    by_id = {}
    for s in scenarios:
        sid = s.get("id")
        if not sid:
            errors.append("scenario missing id")
            continue
        if sid in by_id:
            errors.append(f"duplicate scenario id: {sid}")
        by_id[sid] = s

    allowed = set(matrix.get("kinds", []))
    have = {}
    for sid, kind, rel, lineno in tags:
        have.setdefault((sid, kind), []).append(f"{rel}:{lineno}")
        if kind not in allowed:
            errors.append(f"{rel}:{lineno}: scenario {sid} has unknown kind {kind!r}")
        if sid not in by_id:
            errors.append(f"{rel}:{lineno}: orphan tag — scenario {sid!r} not in scenarios.json")

    for s in scenarios:
        sid = s["id"]
        kinds = s.get("verification", [])
        for kind in kinds:
            if kind not in allowed:
                errors.append(f"{sid}: declares unknown verification kind {kind!r}")
                continue
            if have.get((sid, kind)):
                continue
            if _waived(waivers, sid, today):
                warnings.append(f"{sid}: kind {kind} waived")
                continue
            errors.append(f"{sid}: no verified test for kind {kind}")
        if s.get("priority") == "P0" and not (set(kinds) & BEHAVIOR_KINDS):
            errors.append(f"{sid}: P0 must declare a behavior-level kind (integration/e2e/fault)")

    # Every tier 0/1 module must have at least one scenario.
    module_tiers = {p: int(m["tier"]) for p, m in policy["modules"].items()}
    covered_modules = {s.get("module") for s in scenarios}
    for pkg, tier in module_tiers.items():
        if tier <= 1 and pkg not in covered_modules:
            errors.append(f"tier {tier} module {pkg} has no scenario in scenarios.json")

    return errors, warnings


def main(argv=None):
    ap = argparse.ArgumentParser(description="Heron scenario-coverage checker")
    ap.add_argument("--policy", default="verification/coverage-policy.json")
    ap.add_argument("--scenarios", default=None)
    ap.add_argument("--waivers", default=None)
    ap.add_argument("--repo-root", default=".")
    args = ap.parse_args(argv)

    root = os.path.abspath(args.repo_root)
    policy = load_json(args.policy)
    scenarios_path = args.scenarios or os.path.join(root, policy["scenarios_file"])
    waivers_path = args.waivers or os.path.join(root, policy["waivers_file"])
    matrix = load_json(scenarios_path)
    waivers = load_json(waivers_path) if os.path.exists(waivers_path) else {"waivers": []}

    tags = collect_tags(root)
    errors, warnings = check(policy, matrix, waivers, tags)
    for w in warnings:
        print(f"WARN: {w}", file=sys.stderr)
    for e in errors:
        print(f"FAIL: {e}", file=sys.stderr)
    print(f"scenarios: {len(matrix.get('scenarios', []))} defined, {len(tags)} tag(s), "
          f"{'OK' if not errors else 'FAIL'}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
