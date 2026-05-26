#!/usr/bin/env python3
"""Validated-constructor caller-audit linter.

Catches the PR#47 failure class: a struct gains a validating
constructor function (e.g. `to_time_range(start, end) -> Result<TimeRange, …>`),
but some caller keeps building the struct directly with `TimeRange { … }`
and bypasses the validation. The validator's job — clamping inputs,
rejecting out-of-range values — never runs for that caller, and the
bug shows up as a runtime 500 instead of a compile error.

Config file: `scripts/lint/validated-types.txt`
    One line per validated type. Format:

        # comment
        TypeName : glob1 glob2 ...

    `TypeName` is the Rust type name. `glob1 glob2 …` are file globs
    (relative to repo root) where direct `TypeName { … }` construction
    is intentionally allowed — typically the constructor's own file
    plus tests.

The linter scans `server/**/*.rs` for struct-literal constructions of
each listed type and reports any hit outside the allow-list.

Heuristic detection
-------------------
A "struct literal" hit is a line matching
    `\\b<Type>\\s*\\{\\s*(?:\\.\\.[\\w_]+|[\\w_]+\\s*:)`
i.e. `TypeName {` followed by either a field-colon or struct-update
syntax. Lines that look like declarations (start with `struct`, `enum`,
`impl`, `trait`, or `union`) are excluded. Doc comments and string
literals are excluded.

Exit codes
----------
    0 = clean
    1 = violations found
    2 = configuration / invocation error
"""
from __future__ import annotations

import fnmatch
import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CONFIG = REPO_ROOT / "scripts" / "lint" / "validated-types.txt"
SCAN_GLOBS = ["server/**/*.rs"]

DECL_KEYWORDS = ("struct ", "enum ", "impl ", "trait ", "union ")


def load_config() -> list[tuple[str, list[str]]]:
    if not CONFIG.exists():
        print(
            f"check-validated-constructors: no config at {CONFIG.relative_to(REPO_ROOT)}; nothing to lint."
        )
        sys.exit(0)
    rules: list[tuple[str, list[str]]] = []
    for raw in CONFIG.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if ":" not in line:
            print(
                f"check-validated-constructors: malformed config line (missing ':'): {raw!r}",
                file=sys.stderr,
            )
            sys.exit(2)
        type_name, _, globs_part = line.partition(":")
        type_name = type_name.strip()
        globs = [g for g in globs_part.split() if g]
        if not type_name:
            print(
                f"check-validated-constructors: empty type name in line {raw!r}",
                file=sys.stderr,
            )
            sys.exit(2)
        rules.append((type_name, globs))
    return rules


def find_rs_files() -> list[Path]:
    files: list[Path] = []
    for pattern in SCAN_GLOBS:
        files.extend(REPO_ROOT.glob(pattern))
    # Filter out target/, vendor/, etc.
    return [
        f
        for f in files
        if "target" not in f.parts and "node_modules" not in f.parts
    ]


def line_is_declaration(line: str) -> bool:
    stripped = line.lstrip()
    return any(stripped.startswith(k) or stripped.startswith("pub " + k) for k in DECL_KEYWORDS)


def is_string_literal_context(line: str, idx: int) -> bool:
    """Cheap check: is position idx inside a `"..."` literal on this line?"""
    quote_count = 0
    for i, ch in enumerate(line[:idx]):
        if ch == '"' and (i == 0 or line[i - 1] != "\\"):
            quote_count += 1
    return quote_count % 2 == 1


def compute_test_mod_regions(text: str) -> list[tuple[int, int]]:
    """Return [start, end) byte ranges of `#[cfg(test)] mod … { … }`
    blocks. Hits inside these regions are intentional test-only code
    and not subject to the validation rule.

    Heuristic: find each `#[cfg(test)]` line that has `mod ` after it
    (skipping whitespace and the attribute). From the `{` that follows
    the `mod` name, track brace depth and record the matching `}`.
    String literals and char literals are tracked to avoid counting
    braces inside them. Comments are stripped lazily — only `// …` line
    comments and `/* … */` block comments at file-scope are filtered.
    Worst case the detector mis-counts inside a regex or macro
    literal; the consequence is a few legitimate hits being suppressed
    or a single noisy violation, neither catastrophic for a linter
    that runs in CI."""
    regions: list[tuple[int, int]] = []
    # Find every `#[cfg(test)]` annotation.
    for m in re.finditer(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]", text):
        # Find next `mod <name>` after the annotation.
        rest = text[m.end():]
        mod_match = re.search(r"\s*mod\s+\w+\s*\{", rest)
        if not mod_match:
            continue
        open_brace = m.end() + mod_match.end() - 1
        # Walk forward tracking brace depth, ignoring braces in strings/comments.
        depth = 1
        i = open_brace + 1
        n = len(text)
        in_str = False
        in_char = False
        in_line_comment = False
        in_block_comment = False
        while i < n and depth > 0:
            ch = text[i]
            nxt = text[i + 1] if i + 1 < n else ""
            if in_line_comment:
                if ch == "\n":
                    in_line_comment = False
            elif in_block_comment:
                if ch == "*" and nxt == "/":
                    in_block_comment = False
                    i += 1
            elif in_str:
                if ch == "\\":
                    i += 1  # skip escape
                elif ch == '"':
                    in_str = False
            elif in_char:
                if ch == "\\":
                    i += 1
                elif ch == "'":
                    in_char = False
            else:
                if ch == "/" and nxt == "/":
                    in_line_comment = True
                    i += 1
                elif ch == "/" and nxt == "*":
                    in_block_comment = True
                    i += 1
                elif ch == '"':
                    in_str = True
                elif ch == "'":
                    # Lifetime ('a, 'static) starts with ' but isn't a char
                    # literal. Distinguish by checking the next two chars.
                    if i + 2 < n and text[i + 2] == "'":
                        in_char = True
                    elif i + 3 < n and text[i + 1] == "\\":
                        in_char = True
                    # else: lifetime — ignore
                elif ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                    if depth == 0:
                        regions.append((open_brace, i + 1))
                        break
            i += 1
    return regions


def in_test_mod(pos: int, regions: list[tuple[int, int]]) -> bool:
    for s, e in regions:
        if s <= pos < e:
            return True
    return False


def scan_file(path: Path, rules: list[tuple[str, list[str]]]) -> list[tuple[str, int, str, str]]:
    """Returns list of (type_name, line_no, file_rel, line_text) violations
    (allow-list filtering happens later).

    Multi-line struct literals are common in Rust:

        TimeRange {
            start_us: ...,
            end_us: ...,
        }

    so the detector scans the whole file as one DOTALL string and
    looks for `Type {` followed by struct-update (`..base`) or any
    field-colon within ~200 characters (handles realistic formatting
    while keeping the regex cheap)."""
    hits: list[tuple[str, int, str, str]] = []
    try:
        text = path.read_text()
    except (UnicodeDecodeError, OSError):
        return hits
    rel = str(path.relative_to(REPO_ROOT))
    test_regions = compute_test_mod_regions(text)
    # Precompute newline offsets so we can map regex match position to a line number.
    line_starts = [0]
    for i, ch in enumerate(text):
        if ch == "\n":
            line_starts.append(i + 1)

    def line_no(pos: int) -> int:
        # Binary search would be tidier; linear scan is fine for code files.
        lo, hi = 0, len(line_starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if line_starts[mid] <= pos:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    lines = text.splitlines()

    for type_name, _ in rules:
        # `Type` followed (within reasonable distance) by `{` then a
        # field-colon or struct-update marker. Multi-line OK.
        # The `(?!:)` at the end excludes `::` paths like
        # `ts_storage::query::TimeRange { ... }` from being matched
        # at the wrong position; only a single `:` (field separator)
        # counts.
        pat = re.compile(
            r"\b"
            + re.escape(type_name)
            + r"\s*\{\s*(?:\.\.[\w_]+|[\w_]+\s*:(?!:))",
            re.DOTALL,
        )
        for m in pat.finditer(text):
            # Hits inside `#[cfg(test)] mod tests { … }` blocks are
            # legitimate test code; skip them.
            if in_test_mod(m.start(), test_regions):
                continue
            ln = line_no(m.start())
            line_text = lines[ln - 1] if ln - 1 < len(lines) else ""
            # Skip declarations (`struct X {`, `impl X {`, etc.).
            if line_is_declaration(line_text):
                continue
            # Skip line-comment hits.
            stripped = line_text.lstrip()
            if stripped.startswith("//"):
                continue
            # Skip if hit is inside a string literal on the START line.
            col = m.start() - line_starts[ln - 1]
            if is_string_literal_context(line_text, col):
                continue
            hits.append((type_name, ln, rel, line_text.rstrip()))
    return hits


def file_allowed(rel_path: str, globs: list[str]) -> bool:
    for g in globs:
        # fnmatch doesn't support `**`; expand it to `*` recursively.
        # Convert `**/x` -> match across any depth.
        # Use Path.match semantics: implement via a simple normalization.
        # Easiest path: split on '/' and walk.
        if _glob_match(rel_path, g):
            return True
    return False


def _glob_match(rel_path: str, pattern: str) -> bool:
    # Path.match supports `**` semantics for `Path`. We use pathlib.
    return Path(rel_path).match(pattern) or fnmatch.fnmatch(rel_path, pattern)


def main() -> int:
    rules = load_config()
    if not rules:
        print("check-validated-constructors: config has no rules; nothing to lint.")
        return 0

    all_files = find_rs_files()
    allowlist_by_type: dict[str, list[str]] = {t: globs for t, globs in rules}

    violations: list[tuple[str, int, str, str]] = []
    for f in all_files:
        for hit in scan_file(f, rules):
            type_name, _ln, rel, _txt = hit
            if file_allowed(rel, allowlist_by_type[type_name]):
                continue
            violations.append(hit)

    if not violations:
        type_list = ", ".join(t for t, _ in rules)
        print(
            f"check-validated-constructors: ✓ no unscoped constructions of: {type_list}"
        )
        return 0

    print(
        f"::error::check-validated-constructors: {len(violations)} direct construction(s) bypass validator(s)"
    )
    print()
    print("Violations (each must either move to the validating constructor")
    print("or be added to scripts/lint/validated-types.txt allow-list):")
    by_type: dict[str, list[tuple[int, str, str]]] = {}
    for type_name, ln, rel, txt in violations:
        by_type.setdefault(type_name, []).append((ln, rel, txt))
    for type_name in sorted(by_type):
        print(f"  {type_name}:")
        for ln, rel, txt in by_type[type_name]:
            print(f"    {rel}:{ln}: {txt.strip()}")
    print()
    print("Why this matters: when a validator function is added for a")
    print("struct (e.g. to_time_range for TimeRange), every caller that")
    print("still constructs the struct directly silently bypasses the")
    print("validation. The bug then surfaces only at runtime, often as a")
    print("500 in a different code path than the one being changed. This")
    print("linter forces the constructor to be the single entry point.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
