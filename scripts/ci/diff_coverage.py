#!/usr/bin/env python3
"""Changed-code coverage gate — the primary per-PR check in
docs/design/11-verification.md.

Intersects a coverage report with the lines a PR actually changed and enforces
the owning tier's `changed_code` thresholds. This is what stops "add code now,
tests later": repository floors may be aspirational for legacy code, but new
code has to be verified.

Inputs (all optional except lcov):
  --lcov       lcov file from `cargo llvm-cov --lcov` (line coverage)
  --cov-json   `cargo llvm-cov --json` export (region coverage, when available)
  --base       git base ref (default origin/main); merge-base is used
  --paths      path filters for the diff (default: server console/src)

Only changed lines that appear in the coverage report are counted (comments,
blank lines and attributes have no DA record). A changed file absent from the
report is reported as unmeasured, not silently passed.

Exit 0 = pass, 1 = below threshold, 2 = usage/IO.
"""
import argparse
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_coverage_policy import (  # noqa: E402
    load_json,
    package_for_file,
    relpath,
    tier_for_file,
)

HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
LCOV_DA_RE = re.compile(r"^DA:(\d+),(\d+)")


# --------------------------------------------------------------------------
# pure parsers
# --------------------------------------------------------------------------
def parse_lcov(text):
    """{file: {line: hit_count}} from lcov (SF/DA records)."""
    out = {}
    cur = None
    for line in text.splitlines():
        if line.startswith("SF:"):
            cur = line[3:].strip()
            out.setdefault(cur, {})
        elif line.startswith("DA:") and cur is not None:
            m = LCOV_DA_RE.match(line)
            if m:
                out[cur][int(m.group(1))] = int(m.group(2))
        elif line == "end_of_record":
            cur = None
    return out


def parse_regions(report):
    """{file: {line: covered_bool}} from a cargo-llvm-cov JSON export.

    Region entries are the `segments` with isRegionEntry (index 4) and not a gap
    (index 5); the execution count at index 2 decides covered.
    """
    out = {}
    for entry in report.get("data", []):
        for f in entry.get("files", []):
            lines = {}
            for seg in f.get("segments", []):
                if len(seg) < 5 or not seg[4] or (len(seg) > 5 and seg[5]):
                    continue
                ln, count = int(seg[0]), int(seg[2])
                prev = lines.get(ln, False)
                lines[ln] = prev or count > 0
            if lines:
                out[f["filename"]] = lines
    return out


def parse_diff(diff_text):
    """{repo-relative path: set(added line numbers)} from `git diff -U0`."""
    out = {}
    path = None
    for line in diff_text.splitlines():
        if line.startswith("+++ "):
            target = line[4:].strip()
            if target == "/dev/null":
                path = None
                continue
            path = target[2:] if target.startswith("b/") else target
            out.setdefault(path, set())
        elif line.startswith("@@ ") and path is not None:
            m = HUNK_RE.match(line)
            if not m:
                continue
            start = int(m.group(1))
            count = int(m.group(2)) if m.group(2) is not None else 1
            out[path].update(range(start, start + count))
    return {p: ls for p, ls in out.items() if ls}


def _is_gated_path(rel):
    """Only production source is gated. Config files are not instrumentable;
    integration tests are separate binaries that the report does not cover;
    TS test files are their own thing."""
    if not rel.endswith((".rs", ".ts", ".tsx")):
        return False
    if rel.endswith("build.rs") or rel.endswith(".test.ts") or rel.endswith(".test.tsx"):
        return False
    parts = rel.split("/")
    return "tests" not in parts


# --------------------------------------------------------------------------
# evaluation
# --------------------------------------------------------------------------
def evaluate(changed, lcov, regions, policy):
    """Per-tier changed line/region coverage.

    Returns (results, unmeasured_files) where results[tier] =
    {"line": {"covered","total"}, "region": {...}}.
    """
    thresholds = policy.get("changed_code", {})
    results = {}
    unmeasured = []

    def bucket(tier):
        return results.setdefault(tier, {"line": {"covered": 0, "total": 0},
                                         "region": {"covered": 0, "total": 0}})

    for rel, added in changed.items():
        tier = tier_for_file(rel, policy)
        if tier is None or not thresholds.get(str(tier)):
            continue
        b = bucket(tier)

        line_map = lcov.get(rel)
        if line_map is None:
            unmeasured.append(rel)
        else:
            instrumentable = added & set(line_map)
            b["line"]["total"] += len(instrumentable)
            b["line"]["covered"] += sum(1 for ln in instrumentable if line_map[ln] > 0)

        region_map = regions.get(rel)
        if region_map:
            rl = added & set(region_map)
            b["region"]["total"] += len(rl)
            b["region"]["covered"] += sum(1 for ln in rl if region_map[ln])
    return results, unmeasured


def verdicts(results, policy):
    thresholds = policy.get("changed_code", {})
    ok, details = True, []
    for tier in sorted(results):
        t = results[tier]
        th = thresholds.get(str(tier), {})
        for metric in ("line", "region"):
            total = t[metric]["total"]
            if total == 0:
                continue
            pct = 100.0 * t[metric]["covered"] / total
            target = th.get(metric)
            if target is None:
                continue
            good = pct + 1e-9 >= float(target)
            ok = ok and good
            details.append({"tier": tier, "metric": metric, "pct": round(pct, 2),
                            "target": target, "covered": t[metric]["covered"],
                            "total": total, "ok": good})
    return ok, details


# --------------------------------------------------------------------------
# git
# --------------------------------------------------------------------------
def _run(args, cwd):
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True)


def changed_lines(repo_root, base, paths):
    """(changed dict, base_sha) or raises RuntimeError with a clear reason."""
    mb = _run(["git", "merge-base", base, "HEAD"], repo_root)
    if mb.returncode != 0:
        raise RuntimeError(f"git merge-base {base} HEAD failed: {mb.stderr.strip()}")
    base_sha = mb.stdout.strip()
    spec = [f"{base_sha}...HEAD", "--unified=0", "--no-color", "--"] + list(paths)
    d = _run(["git", "diff"] + spec, repo_root)
    if d.returncode != 0:
        raise RuntimeError(f"git diff failed: {d.stderr.strip()}")
    return parse_diff(d.stdout), base_sha


def _markdown(results, details, unmeasured, base_sha):
    lines = ["### Changed-code coverage", "",
             f"Base: `{base_sha[:12]}`", "",
             "| Tier | Metric | Coverage | Target | Changed | Status |",
             "|---|---|---|---|---|---|"]
    for d in details:
        icon = "✅" if d["ok"] else "❌"
        lines.append(f"| {d['tier']} | {d['metric']} | {d['pct']}% | {d['target']}% | "
                     f"{d['covered']}/{d['total']} | {icon} |")
    if unmeasured:
        lines += ["", "Unmeasured changed files (not in the coverage report):", ""]
        lines += [f"- `{f}`" for f in sorted(unmeasured)]
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------
# entry point
# --------------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(description="Heron changed-code coverage gate")
    ap.add_argument("--lcov", required=True)
    ap.add_argument("--cov-json", default=None)
    ap.add_argument("--base", default="origin/main")
    ap.add_argument("--paths", nargs="*", default=["server", "console/src"])
    ap.add_argument("--policy", default="verification/coverage-policy.json")
    ap.add_argument("--repo-root", default=".")
    ap.add_argument("--markdown", default=None)
    args = ap.parse_args(argv)

    root = os.path.abspath(args.repo_root)
    policy = load_json(args.policy)

    changed, base_sha = changed_lines(root, args.base, args.paths)
    changed = {p: ls for p, ls in changed.items()
               if _is_gated_path(p) and package_for_file(p, policy)}

    lcov_raw = parse_lcov(open(args.lcov, encoding="utf-8").read())
    lcov = {relpath(k, root): v for k, v in lcov_raw.items()}
    regions = {}
    if args.cov_json:
        regions = {relpath(k, root): v
                   for k, v in parse_regions(load_json(args.cov_json)).items()}

    results, unmeasured = evaluate(changed, lcov, regions, policy)
    ok, details = verdicts(results, policy)

    print(f"changed-code coverage vs {base_sha[:12]} ({len(changed)} file(s))")
    if not details:
        print("  no changed instrumentable lines in polled tiers — nothing to gate")
    for d in details:
        flag = "ok  " if d["ok"] else "FAIL"
        print(f"  [{flag}] tier {d['tier']} {d['metric']}: {d['pct']}% "
              f"(target {d['target']}%, {d['covered']}/{d['total']})")
    for f in sorted(unmeasured):
        print(f"  WARN unmeasured changed file: {f}", file=sys.stderr)

    if args.markdown:
        with open(args.markdown, "w", encoding="utf-8") as fh:
            fh.write(_markdown(results, details, unmeasured, base_sha))

    if not ok:
        print("changed-code coverage: FAIL", file=sys.stderr)
        return 1
    print("changed-code coverage: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
