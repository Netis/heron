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

# Authors whose PRs the AI is allowed to auto-merge on APPROVE. The
# AI's review still has to land first, and the review still has to
# go through the same `pick_event` thresholding — but if the verdict
# is APPROVE and the PR was opened by a trusted author, we squash
# and delete the branch right after posting.
AUTO_MERGE_AUTHORS = {
    a.strip() for a in os.environ.get("AUTO_MERGE_AUTHORS", "vaderyang").split(",") if a.strip()
}


def read_review() -> str:
    if not OUT_PATH.exists():
        return ""
    return OUT_PATH.read_text(errors="replace").strip()


def section_nonempty(body: str, heading: str) -> bool:
    """True if `## <heading>` or `### <heading>` exists and has at least
    one non-blank line of content before the next `## `/`### ` heading or
    end-of-document."""
    # Match either ## or ### for the heading itself
    pat = re.compile(
        rf"^(?:###?)\s+{re.escape(heading)}\s*\n(.*?)(?=^(?:###?)\s+|\Z)",
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
      * agent run failed OR body has no markdown structure → COMMENT
        (never approve a failed run)
      * agent explicitly said "REQUEST_CHANGES" / "APPROVE" / "COMMENT"
        in the Summary → trust the agent
      * else: Blocking → REQUEST_CHANGES; Suggestions/Questions only →
        COMMENT; nothing → APPROVE.
    """
    # Hard guard: if the agent itself bailed out (workflow step failed)
    # or wrote a bare ERROR line, do NOT issue an APPROVE — the body
    # may not have any of the structured sections below and would
    # otherwise fall through to the default-APPROVE branch.
    if AGENT_EXIT == "failure":
        return "COMMENT"
    stripped = body.lstrip()
    if stripped.startswith("ERROR:") or stripped.startswith("ERROR "):
        return "COMMENT"
    if not re.search(r"^(?:###?)\s+\w", body, re.MULTILINE):
        # No markdown headings at all — treat as a free-form note,
        # not a verdict.
        return "COMMENT"

    # Look for Summary section (## Summary or ### Summary)
    summary_pat = re.compile(
        r"^(?:###?)\s+Summary\s*\n(.*?)(?=^(?:###?)\s+|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = summary_pat.search(body)
    summary = (m.group(1) if m else "").upper()
    # An explicit verdict token in the Summary takes precedence over the
    # section-based inference below: the agent stated its intent directly,
    # so trust it even if the surrounding sections would imply otherwise.
    for token in ("REQUEST_CHANGES", "APPROVE", "COMMENT"):
        if token in summary:
            return token

    # Fall back to section-based inference
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


def pr_author(number: str) -> str | None:
    proc = subprocess.run(
        ["gh", "pr", "view", number, "--json", "author", "--jq", ".author.login"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(f"gh pr view (author) failed: {proc.stderr}\n")
        return None
    return proc.stdout.strip() or None


def pr_has_label(number: str, name: str) -> bool:
    proc = subprocess.run(
        ["gh", "pr", "view", number, "--json", "labels", "--jq",
         f'any(.labels[]; .name == "{name}")'],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(f"gh pr view (labels) failed: {proc.stderr}\n")
        return False
    return proc.stdout.strip() == "true"


def auto_merge(number: str) -> None:
    """Squash-merge with admin bypass. Repo doesn't have native
    `--auto` enabled, so we squash inline. Branch is deleted on
    success. Any failure (merge conflict, branch protection
    surprise, etc.) is logged but doesn't fail the workflow — the
    review is already posted, the operator can finish merging by
    hand.

    `--admin` requires a token whose owner has admin rights on the
    repo; the default GITHUB_TOKEN does NOT and the call errors out
    with "Required status check 'test' is expected (mergePullRequest)".
    If ADMIN_GH_TOKEN is set, swap GH_TOKEN to it for this call only.
    """
    cmd = ["gh", "pr", "merge", number, "--admin", "--squash", "--delete-branch"]
    env = os.environ.copy()
    admin_tok = os.environ.get("ADMIN_GH_TOKEN", "").strip()
    if admin_tok:
        env["GH_TOKEN"] = admin_tok
    proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
    if proc.returncode != 0:
        sys.stderr.write(
            f"auto-merge failed (left for manual merge): {proc.stderr}\n"
        )
    else:
        print(f"auto-merged PR #{number}")


def main() -> int:
    # Agent-failure path: keep failure details out of the PR entirely.
    # Operators read workflow logs for diagnostics; PR readers should
    # never see "Agent run failed" / "agent unavailable" / etc. — those
    # comments are noise to anyone watching the PR and leak nothing
    # useful to authors.
    if AGENT_EXIT == "failure":
        print("agent run failed — not posting to PR (see workflow log)")
        return 0

    body = read_review()
    if not body:
        print("no review body — skipping post")
        return 0

    # Bare body if it starts with ERROR — that's a pre-flight signal,
    # also belongs in workflow logs only.
    if body.lstrip().startswith("ERROR"):
        print("agent reported pre-flight error — not posting to PR")
        return 0

    footer = (
        "\n\n---\n"
        "🤖 Reviewed by **vivi** • "
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

    # Auto-merge gate: only fires on APPROVE, only for PRs opened by
    # a trusted author (see AUTO_MERGE_AUTHORS). The thinking is
    # that an APPROVE from the AI is enough signal for low-stakes
    # changes by the project maintainer, but anyone else's PR
    # still gets human review.
    #
    # PRs labelled `auto-agent` are wiwi-spawned; their auto-merge
    # decision is owned by scripts/agent-bot/auto_merge.sh (different
    # gates: linked-issue author, not PR author). Skip here so the
    # two paths never race on the same PR.
    if event == "APPROVE":
        if pr_has_label(PR_NUMBER, "auto-agent"):
            print("PR has `auto-agent` label — leaving auto-merge to agent-bot/auto_merge.sh")
        else:
            author = pr_author(PR_NUMBER)
            if author and author in AUTO_MERGE_AUTHORS:
                print(f"author={author} in AUTO_MERGE_AUTHORS — squash-merging")
                auto_merge(PR_NUMBER)
            else:
                print(f"author={author} not in AUTO_MERGE_AUTHORS={AUTO_MERGE_AUTHORS} — left for human")

    return 0


if __name__ == "__main__":
    sys.exit(main())
