#!/usr/bin/env python3
"""Coverage-policy checker — the deterministic gate behind
docs/design/11-verification.md.

Three jobs, all offline and dependency-free (stdlib only):

  --static              Validate the policy itself: every workspace crate is
                        classified (or explicitly excluded), tiers/overrides are
                        well-formed, and every waiver carries reason + owner +
                        expiry and has not expired.
  --report <cov.json>   Aggregate a `cargo llvm-cov --json` report per crate and
                        enforce (a) no decrease vs the committed baseline, and
                        optionally (--enforce-tiers) the tier target floors.
  --write-baseline <cov.json> [--out PATH]
                        Regenerate the committed baseline from a fresh report.

Day-one blocking is no-decrease + the changed-code gate (diff_coverage.py).
Tier target floors are reported always and enforced only with --enforce-tiers,
so the ratchet can be turned on per the rollout plan without disabling merges.

Exit code: 0 = pass, 1 = policy failure, 2 = usage/IO error.
"""
import argparse
import datetime
import glob
import json
import os
import re
import subprocess
import sys

METRICS = ("line", "region", "function")
SUMMARY_KEY = {"line": "lines", "region": "regions", "function": "functions"}


# --------------------------------------------------------------------------
# loading / discovery
# --------------------------------------------------------------------------
def load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def repo_root_of(path):
    return os.path.abspath(path)


def parse_package_name(cargo_toml):
    """First top-level `name = "..."` in a crate manifest is its package name."""
    with open(cargo_toml, encoding="utf-8") as fh:
        text = fh.read()
    # Restrict to the [package] table so [lib]/[[bin]] names can't win.
    m = re.search(r"^\[package\]\s*$", text, re.MULTILINE)
    section = text[m.end():] if m else text
    nxt = re.search(r"^\[", section, re.MULTILINE)
    if nxt:
        section = section[:nxt.start()]
    name = re.search(r'^\s*name\s*=\s*"([^"]+)"', section, re.MULTILINE)
    return name.group(1) if name else None


def discover_packages(repo_root):
    """{package_name: repo-relative dir} for every host workspace member."""
    out = {}
    for pattern in ("server/h-*", "server/app/*"):
        for d in sorted(glob.glob(os.path.join(repo_root, pattern))):
            ct = os.path.join(d, "Cargo.toml")
            if os.path.isfile(ct):
                name = parse_package_name(ct)
                if name:
                    out[name] = os.path.relpath(d, repo_root)
    return out


def relpath(filename, repo_root):
    p = filename.replace("\\", "/")
    root = repo_root.replace("\\", "/").rstrip("/") + "/"
    if p.startswith(root):
        p = p[len(root):]
    return p


def _under(rel, prefix):
    prefix = prefix.rstrip("/")
    return rel == prefix or rel.startswith(prefix + "/")


def package_for_file(rel, policy):
    """Longest module `path` prefix match → package name (or None)."""
    best, best_len = None, -1
    for pkg, meta in policy["modules"].items():
        path = meta.get("path")
        if path and _under(rel, path) and len(path) > best_len:
            best, best_len = pkg, len(path)
    return best


def tier_for_file(rel, policy, package=None):
    """Overrides win (longest prefix); else the module tier."""
    best, best_len = None, -1
    for ov in policy.get("overrides", []):
        if _under(rel, ov["path"]) and len(ov["path"]) > best_len:
            best, best_len = int(ov["tier"]), len(ov["path"])
    if best is not None:
        return best
    pkg = package or package_for_file(rel, policy)
    meta = policy["modules"].get(pkg, {})
    return int(meta.get("tier", 3)) if meta else None


# --------------------------------------------------------------------------
# report aggregation
# --------------------------------------------------------------------------
def aggregate_report(report, repo_root, policy):
    """{package: {metric: {covered,count,percent}}} from an llvm-cov JSON export."""
    pkgs = {}
    for entry in report.get("data", []):
        for f in entry.get("files", []):
            rel = relpath(f["filename"], repo_root)
            pkg = package_for_file(rel, policy)
            if not pkg:
                continue
            agg = pkgs.setdefault(pkg, {m: {"covered": 0, "count": 0} for m in METRICS})
            summary = f.get("summary", {})
            for metric in METRICS:
                s = summary.get(SUMMARY_KEY[metric], {}) or {}
                agg[metric]["covered"] += int(s.get("covered", 0))
                agg[metric]["count"] += int(s.get("count", 0))
    out = {}
    for pkg, agg in pkgs.items():
        out[pkg] = {}
        for metric in METRICS:
            c, n = agg[metric]["covered"], agg[metric]["count"]
            out[pkg][metric] = round(100.0 * c / n, 2) if n else 0.0
    return out


def make_baseline(cov_pkgs, repo_root):
    commit = "unknown"
    try:
        commit = subprocess.check_output(
            ["git", "-C", repo_root, "rev-parse", "HEAD"], text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except Exception:
        pass
    return {
        "_comment": "Generated by scripts/ci/check_coverage_policy.py --write-baseline. Do not hand-edit; the ratchet is enforced against this file.",
        "commit": commit,
        "generated_at": datetime.date.today().isoformat(),
        "crates": cov_pkgs,
    }


# --------------------------------------------------------------------------
# checks
# --------------------------------------------------------------------------
def check_static(policy, waivers, packages, today=None):
    today = today or datetime.date.today()
    errors = []
    tiers = policy.get("tiers", {})
    modules = policy.get("modules", {})
    exclusions = policy.get("exclusions", {})

    for pkg in packages:
        if pkg not in modules and pkg not in exclusions:
            errors.append(f"unclassified workspace crate: {pkg} (add it to modules or exclusions)")
        if pkg in modules and pkg in exclusions:
            errors.append(f"{pkg} is both classified and excluded")

    for pkg, meta in modules.items():
        if str(meta.get("tier")) not in tiers:
            errors.append(f"{pkg}: unknown tier {meta.get('tier')!r}")
        if not meta.get("path"):
            errors.append(f"{pkg}: missing path")

    for ov in policy.get("overrides", []):
        if str(ov.get("tier")) not in tiers:
            errors.append(f"override {ov.get('path')}: unknown tier {ov.get('tier')!r}")

    for t, meta in policy.get("changed_code", {}).items():
        if t not in tiers:
            errors.append(f"changed_code tier {t!r} has no tier definition")

    for i, w in enumerate(waivers.get("waivers", [])):
        label = w.get("scenario") or w.get("module") or f"#{i}"
        for field in ("reason", "owner", "expires", "tracking"):
            if not w.get(field):
                errors.append(f"waiver {label}: missing {field}")
        exp = w.get("expires")
        if exp:
            try:
                d = datetime.date.fromisoformat(exp)
                if d < today:
                    errors.append(f"waiver {label}: expired on {exp}")
            except ValueError:
                errors.append(f"waiver {label}: malformed expires {exp!r} (want YYYY-MM-DD)")
    return errors


def _waived(waivers, module, metric, today):
    # A `metric: "tier"` waiver excuses every floor metric for that module at
    # once (a crate-level ratchet exception with a single reason + expiry),
    # rather than forcing one entry per metric.
    for w in waivers.get("waivers", []):
        if w.get("module") != module or w.get("metric") not in (metric, "tier"):
            continue
        try:
            if datetime.date.fromisoformat(w["expires"]) >= today:
                return True
        except (KeyError, ValueError):
            continue
    return False


def check_report(policy, baseline, waivers, measured, enforce_tiers=False, today=None):
    today = today or datetime.date.today()
    slack = float(policy.get("slack", 0.5))
    tiers = policy.get("tiers", {})
    errors, warnings = [], []

    for pkg, base in baseline.get("crates", {}).items():
        if pkg not in measured:
            warnings.append(f"{pkg}: no instrumented files in report (skipped)")
            continue
        for metric in METRICS:
            if metric not in base:
                continue
            drop = float(base[metric]) - float(measured[pkg][metric])
            if drop > slack:
                errors.append(
                    f"{pkg}: {metric} coverage dropped {base[metric]}% -> "
                    f"{measured[pkg][metric]}% (slack {slack})"
                )

    if enforce_tiers:
        for pkg, meta in policy["modules"].items():
            if pkg not in measured:
                continue
            tier = str(meta["tier"])
            for metric in METRICS:
                target = float(tiers[tier].get(metric, 0))
                if target <= 0:
                    continue
                if measured[pkg][metric] + slack < target:
                    if not _waived(waivers, pkg, metric, today):
                        errors.append(
                            f"{pkg}: {metric} {measured[pkg][metric]}% < tier {tier} "
                            f"target {target}% (no waiver)"
                        )
    else:
        for pkg, meta in policy["modules"].items():
            if pkg not in measured:
                continue
            tier = str(meta["tier"])
            miss = [
                f"{m}={measured[pkg][m]}%/{tiers[tier].get(m)}%"
                for m in METRICS
                if float(tiers[tier].get(m, 0)) > 0
                and measured[pkg][m] + slack < float(tiers[tier][m])
            ]
            if miss:
                warnings.append(f"{pkg} (tier {tier}) below target: {', '.join(miss)}")
    return errors, warnings


def _table(policy, measured):
    lines = [f"{'crate':24} {'tier':>4} {'line':>8} {'region':>8} {'fn':>8}"]
    for pkg in sorted(measured):
        tier = policy["modules"].get(pkg, {}).get("tier", "?")
        m = measured[pkg]
        lines.append(
            f"{pkg:24} {str(tier):>4} {m['line']:>7}% {m['region']:>7}% {m['function']:>7}%"
        )
    return "\n".join(lines)


# --------------------------------------------------------------------------
# entry point
# --------------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(description="Heron coverage-policy checker")
    ap.add_argument("--static", action="store_true", help="validate policy + waivers only")
    ap.add_argument("--report", metavar="COV_JSON", help="cargo-llvm-cov JSON export")
    ap.add_argument("--write-baseline", metavar="COV_JSON", help="regenerate the baseline")
    ap.add_argument("--out", default=None, help="output path for --write-baseline")
    ap.add_argument("--policy", default="verification/coverage-policy.json")
    ap.add_argument("--baseline", default=None)
    ap.add_argument("--waivers", default=None)
    ap.add_argument("--repo-root", default=".")
    ap.add_argument("--enforce-tiers", action="store_true")
    args = ap.parse_args(argv)

    root = repo_root_of(args.repo_root)
    policy = load_json(args.policy)
    baseline_path = args.baseline or os.path.join(root, policy["baseline_file"])
    waivers_path = args.waivers or os.path.join(root, policy["waivers_file"])
    waivers = load_json(waivers_path) if os.path.exists(waivers_path) else {"waivers": []}
    packages = discover_packages(root)

    if args.static:
        errors = check_static(policy, waivers, packages)
        for e in errors:
            print(f"FAIL: {e}", file=sys.stderr)
        print(f"coverage-policy: {len(packages)} crates, {len(policy['modules'])} classified, "
              f"{len(policy.get('exclusions', {}))} excluded, "
              f"{len(waivers.get('waivers', []))} waiver(s) — "
              f"{'OK' if not errors else 'FAIL'}")
        return 1 if errors else 0

    if args.write_baseline:
        report = load_json(args.write_baseline)
        measured = aggregate_report(report, root, policy)
        out = args.out or baseline_path
        with open(out, "w", encoding="utf-8") as fh:
            json.dump(make_baseline(measured, root), fh, indent=2, sort_keys=True)
            fh.write("\n")
        print(f"wrote baseline: {out} ({len(measured)} crates)")
        return 0

    if args.report:
        report = load_json(args.report)
        measured = aggregate_report(report, root, policy)
        print(_table(policy, measured))
        baseline = load_json(baseline_path)
        errors, warnings = check_report(
            policy, baseline, waivers, measured, enforce_tiers=args.enforce_tiers
        )
        for w in warnings:
            print(f"WARN: {w}", file=sys.stderr)
        for e in errors:
            print(f"FAIL: {e}", file=sys.stderr)
        print(f"coverage-policy report: {'OK' if not errors else 'FAIL'}"
              + ("" if args.enforce_tiers else " (tier targets advisory; use --enforce-tiers)"))
        return 1 if errors else 0

    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
