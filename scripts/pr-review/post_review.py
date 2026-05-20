#!/usr/bin/env python3
"""Post the agent's review markdown to the PR.

Reads /tmp/pr-review-${PR_NUMBER}-out.md, parses for sections, picks
the review event (`APPROVE` / `COMMENT` / `REQUEST_CHANGES`), and
hands it to `gh pr review`. Falls back to a plain comment if `gh pr
review` is unavailable (e.g. the bot account lacks review rights on
its own PRs).

Always exits 0 — failing to post a review shouldn't fail the
workflow run; the workflow logs already capture the agent output.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

PR_NUMBER = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("PR_NUMBER")
if not PR_NUMBER:
    print("ERROR: PR_NUMBER missing", file=sys.stderr)
    sys.exit(0)

OUT_PATH = Path(f"/tmp/pr-review-{PR_NUMBER}-out.md")
RUN_URL = os.environ.get("RUN_URL", "")
AGENT_EXIT = os.environ.get("AGENT_EXIT", "")  # "success" / "failure" / ""


def read_review() -> str:
    if not OUT_PATH.exists():
        return ""
    return OUT_PATH.read_text(errors="replace").strip()


def section_nonempty(body: str, heading: str) -> bool:
    """True if `### <heading>` exists and has at least one non-blank
    line of content before the next `### ` or end-of-document."""
    pat = re.compile(
        rf"^###\s+{re.escape(heading)}\s*\n(.*?)(?=^###\s+|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = pat.search(body)
    if not m:
        return False
    inner = m.group(1).strip()
    return bool(inner)


def pick_event(body: str) -> str:
    """Choose the gh-review event from section presence.

    Priority:
      * agent explicitly said "REQUEST_CHANGES" / "APPROVE" / "COMMENT"
        in the Summary → trust the agent
      * else: Blocking → REQUEST_CHANGES; Suggestions/Questions only →
        COMMENT; nothing → APPROVE.
    """
    summary_pat = re.compile(
        r"^###\s+Summary\s*\n(.*?)(?=^###\s+|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = summary_pat.search(body)
    summary = (m.group(1) if m else "").upper()
    for token in ("REQUEST_CHANGES", "APPROVE", "COMMENT"):
        if token in summary:
            return token
    if section_nonempty(body, "Blocking"):
        return "REQUEST_CHANGES"
    if section_nonempty(body, "Suggestions") or section_nonempty(body, "Questions"):
        return "COMMENT"
    return "APPROVE"


EVENT_FLAG = {
    "APPROVE": "--approve",
    "COMMENT": "--comment",
    "REQUEST_CHANGES": "--request-changes",
}


def post_via_gh_review(number: str, event: str, body: str) -> int:
    cmd = [
        "gh", "pr", "review", number,
        EVENT_FLAG[event],
        "--body", body,
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(
            f"gh pr review failed (event={event}): {proc.stderr}\n"
        )
    return proc.returncode


def post_via_comment(number: str, body: str) -> int:
    cmd = ["gh", "pr", "comment", number, "--body", body]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(f"gh pr comment failed: {proc.stderr}\n")
    return proc.returncode


def main() -> int:
    body = read_review()
    if not body:
        if AGENT_EXIT == "failure":
            body = (
                "### Summary\n"
                "Agent run failed before producing output. "
                f"See [workflow run]({RUN_URL}) for the agent log."
            )
        else:
            print("no review body — skipping post")
            return 0

    footer = (
        "\n\n---\n"
        "🤖 Reviewed by **glm-5** via LiteLLM • "
        f"[workflow run]({RUN_URL})"
    )
    full = body + footer

    event = pick_event(body)
    print(f"posting review event={event} ({len(full)} bytes)")

    # gh pr review refuses to let the same user `--approve` their own
    # PR. Fall back to a plain comment in that case.
    rc = post_via_gh_review(PR_NUMBER, event, full)
    if rc != 0:
        sys.stderr.write("falling back to plain comment\n")
        post_via_comment(PR_NUMBER, full)

    return 0


if __name__ == "__main__":
    sys.exit(main())
