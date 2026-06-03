#!/usr/bin/env bash
# wiwi: dev agent. Branch off main, implement, ensure cargo build + tests
# green, open a DRAFT PR labelled `auto-agent`. Auto-merge gating happens
# downstream in pr-review.yml.
set -euo pipefail

# Auto-merge allowlist (GitHub logins). Sourced from the AUTO_MERGE_TEAM env
# (CSV or whitespace-separated), injected from a repo secret by the workflow
# — kept out of committed source. Used only to add a cosmetic
# "eligible for auto-merge" line to the PR body; the real gate is auto_merge.sh.
TEAM=$(printf '%s' "${AUTO_MERGE_TEAM:-}" | tr ',' ' ')
is_team_member() {
  local who="$1"
  for m in $TEAM; do [ "$m" = "$who" ] && return 0; done
  return 1
}

# Branch name includes a short UTC timestamp so re-runs against the
# same issue don't collide with leftover branches from prior attempts.
STAMP=$(date -u +%Y%m%d-%H%M%S)
BRANCH="agent/wiwi/issue-${ISSUE_NUMBER}-${STAMP}"
git config user.email "wiwi-agent@noreply.local"
git config user.name  "wiwi"
git fetch origin main
git checkout -B "$BRANCH" origin/main

PROMPT=$(mktemp)
cat > "$PROMPT" <<EOF
You are **wiwi**, the dev agent. Implement the change requested by issue
#${ISSUE_NUMBER}. Constraints:

- Stay within the scope the triage agent approved. If you discover the
  task is larger than expected (>300 LOC or cross-cutting), STOP, leave
  a note in /tmp/wiwi-abort.txt explaining why, and exit non-zero.
- Add a deterministic test for the change (unit / integration / a tiny
  fixture). Don't claim done without one.
- After edits, run \`just build\` (or \`cargo check\` + \`bun run build\`
  in console/) — must be green before you stop.
- Do NOT add new dependencies, new secrets, new network calls.
- Do NOT modify CI workflows, branch protection, or this script.
- YOU are responsible for committing your changes. Run
  \`git add -A && git commit -m "..."\` after each logical chunk. The
  driver script will NOT commit for you; if you finish without any
  commits the run is dropped and your work is lost. Use multiple
  commits if it helps reviewers, or one final commit if the change is
  small — but at least one commit MUST exist before you exit.
- The driver script does have a defensive fallback that will
  auto-commit anything you left uncommitted, but DO NOT rely on it —
  the fallback produces a single "wiwi: auto-commit" message that's
  useless to reviewers compared to your own intentional commit
  messages.
- **You may be a RESUMED run.** If \`git log origin/main..HEAD\` shows
  existing commits OR \`git status\` shows uncommitted edits when you
  start, the previous attempt crashed mid-run (typically a LiteLLM /
  GLM-5 hiccup) and the driver is retrying. Treat that state as your
  starting point: do NOT redo work that's already committed; do
  verify the partial state makes sense (read the diffs, build, run
  tests); then continue toward the issue's acceptance criteria. If
  the existing state is incoherent and you cannot make sense of it,
  STOP and write to /tmp/wiwi-abort.txt explaining why so a human
  can intervene.

When done, write a brief summary to /tmp/wiwi-summary.md (Markdown) for
the PR body. End it with the literal line:

  Closes #${ISSUE_NUMBER}

Issue title: ${ISSUE_TITLE}
EOF

# Wait for LiteLLM to be reachable before launching claude. Without
# this, a transient GLM-5 / LiteLLM restart that happens during the
# (often-15+ min) self-hosted runner queue wait kills the run
# immediately and wastes the queue wait + branch-prep work. Caps at
# 30 min by default; configurable via MAX_LITELLM_WAIT_SECONDS.
LITELLM_WAIT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/litellm-wait.sh"
# shellcheck source=../lib/litellm-wait.sh
source "$LITELLM_WAIT"
wait_for_litellm || exit $?

# Stream claude's output to BOTH the workflow log (stdout) and a
# preserved file. Previously this was `> /tmp/wiwi-run.log 2>&1`,
# which silenced the GH Actions log entirely for the run — you
# couldn't tell from outside whether wiwi was making progress,
# looping, or hung. Streaming is essential when the run can take an
# hour: a developer watching `gh run watch` should see each tool
# call land in near-real-time.
#
# `stdbuf -oL` forces line-buffered stdout from claude and tee
# (otherwise the 4KB pipe buffer can hide progress for minutes when
# claude emits small lines slowly).
#
# `tee` always exits 0, so we briefly drop `set -e` and capture
# claude's actual exit via `${PIPESTATUS[0]}`.
#
# Retry on transient LiteLLM mid-stream failures: if claude exits
# non-zero AND the backend now looks down (5xx / connect-refused /
# timeout — not 4xx, which means LiteLLM is choosing to reject and
# waiting won't help), wait for LiteLLM to come back and re-run
# claude from scratch. The prompt above tells claude how to resume
# from the partial state on disk. Up to CLAUDE_RETRY_MAX retries
# (default 2 → 3 attempts total).
CLAUDE_RETRY_MAX="${CLAUDE_RETRY_MAX:-2}"
attempt=0
claude_exit=0
while true; do
    set +e
    stdbuf -oL claude --print \
      --allowed-tools Bash Read Write Edit Grep Glob \
      --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
      < "$PROMPT" 2>&1 | stdbuf -oL tee /tmp/wiwi-run.log
    claude_exit=${PIPESTATUS[0]}
    set -e

    [ "$claude_exit" -eq 0 ] && break

    if ! litellm_appears_down; then
        # LiteLLM is up — failure is something else (claude
        # internal error, prompt issue, token budget). Don't retry.
        break
    fi

    if [ "$attempt" -ge "$CLAUDE_RETRY_MAX" ]; then
        echo "::error::wiwi claude died $((attempt+1)) times with LiteLLM down; giving up" >&2
        break
    fi

    attempt=$((attempt + 1))
    echo "::warning::wiwi claude exited $claude_exit, LiteLLM down; waiting + retrying (attempt $((attempt+1))/$((CLAUDE_RETRY_MAX+1)))" >&2
    wait_for_litellm || break
done

if [ "$claude_exit" != "0" ]; then
  echo "wiwi run failed (claude exit=$claude_exit; see /tmp/wiwi-run.log)" >&2
  gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi could not complete this task. See workflow log."
  exit 1
fi

if [ -f /tmp/wiwi-abort.txt ]; then
  gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi aborted: $(cat /tmp/wiwi-abort.txt)"
  exit 0
fi

# Defensive fallback: if claude left uncommitted edits, salvage them
# rather than losing 30+ minutes of work to the runner workspace
# cleanup. The first 26487120283-class incident burned a full Heron
# rebrand because the prompt said "Commit in logical chunks" and
# claude interpreted that as something the driver would handle. Auto-
# commit message is intentionally terse — reviewer can rewrite the
# message after-the-fact if needed, or wiwi can re-run with the
# tightened prompt.
if ! git diff --quiet || ! git diff --cached --quiet || \
   [ -n "$(git ls-files --others --exclude-standard)" ]; then
  echo "wiwi: detected uncommitted edits at end of run; auto-committing as fallback" >&2
  git add -A
  git commit -m "wiwi: auto-commit uncommitted edits for #${ISSUE_NUMBER}" \
                -m "Generated by run_wiwi.sh fallback. claude finished without committing; this commit captures the working-tree state so the PR isn't empty. Reviewer: consider asking wiwi to re-run with explicit commit guidance if the diff is large or unclear."
fi

# Sanity: must have produced commits.
if [ "$(git rev-list --count origin/main..HEAD)" = "0" ]; then
  gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi finished without any commit OR uncommitted edits; nothing to PR."
  exit 0
fi

git push -u origin "$BRANCH"

BODY_FILE=$(mktemp)
{
  cat /tmp/wiwi-summary.md 2>/dev/null || echo "(wiwi did not write a summary)"
  echo
  echo "---"
  echo "🤖 Implemented by **wiwi** • issue author: @${ISSUE_AUTHOR}"
  if is_team_member "$ISSUE_AUTHOR"; then
    echo "Eligible for auto-merge on vivi APPROVE."
  fi
} > "$BODY_FILE"

gh pr create \
  --draft \
  --base main \
  --head "$BRANCH" \
  --title "${ISSUE_TITLE}" \
  --body-file "$BODY_FILE" \
  --label auto-agent
