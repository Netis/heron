#!/usr/bin/env bash
# wiwi REVISION pass. Dispatched (via pr-revise.yml) when vivi posts a
# CHANGES_REQUESTED review on a wiwi PR (label `auto-agent`). Runs inside a
# checkout of the PR head branch, feeds vivi's blocking feedback + the diff
# to claude, ensures the build is green, then commits + pushes back — which
# re-triggers ci → pr-review (vivi re-review). Mirrors run_wiwi.sh's
# LiteLLM-wait + claude-retry plumbing so the two agents behave identically
# under a flaky backend.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PR="${PR_NUMBER:?PR_NUMBER required}"
REPO="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY required}"

git config user.email "wiwi-agent@noreply.local"
git config user.name  "wiwi"

# --- gather vivi's feedback + the diff for the prompt ---------------------
# Latest CHANGES_REQUESTED review body. Identify vivi the same way
# auto_merge.sh does (author login OR the vivi footer), so attribution
# quirks between bot accounts don't drop the review.
REVIEW_BODY=$(gh api "repos/$REPO/pulls/$PR/reviews" --jq '
  [ .[]
    | select(.user.login=="vivi" or ((.body // "") | contains("vivi")))
    | select(.state=="CHANGES_REQUESTED") ]
  | last | .body // ""' 2>/dev/null || true)

# Inline (file/line) review comments, if any.
INLINE=$(gh api "repos/$REPO/pulls/$PR/comments" --paginate --jq '
  .[] | "- \(.path):\(.line // .original_line // "?") — \((.body // "") | gsub("\n"; " "))"' \
  2>/dev/null || true)

# The diff under review (truncated — the agent also has the working tree).
DIFF=$(gh pr diff "$PR" 2>/dev/null | head -c 60000 || true)

if [ -z "${REVIEW_BODY//[[:space:]]/}" ]; then
  echo "::warning::no CHANGES_REQUESTED review body found for PR #$PR; nothing to revise" >&2
  exit 0
fi

PROMPT=$(mktemp)
cat > "$PROMPT" <<EOF
You are **wiwi**, the dev agent, doing a REVISION pass on PR #${PR}. The
reviewer **vivi** requested changes. The PR's head branch is already checked
out in the current working tree. Address the review here. Constraints:

- Fix EVERY **Blocking** item vivi listed. Apply Suggestions where they are
  cheap and clearly correct; if you deliberately skip one, say why in the
  commit message. Don't argue with the review — change the code (or, when the
  reviewer is factually wrong, fix the misleading code/comment that led them
  there).
- Keep scope to the review. Do NOT introduce unrelated changes, refactors,
  new dependencies, new secrets, or new network calls.
- Do NOT modify CI workflows, branch protection, or the agent-bot scripts.
- Keep and extend deterministic tests. After edits run \`just build\` (or
  \`cargo check\` + \`bun run build\` in console/) — it MUST be green before
  you stop.
- YOU are responsible for committing. Run \`git add -A && git commit -m "..."\`
  with a message describing what you changed in response to the review. At
  least one new commit MUST exist before you exit, or the run is dropped.
- **You may be a RESUMED run** (a prior attempt crashed mid-revision). If
  \`git status\` shows uncommitted edits when you start, treat them as your
  starting point: read the diffs, build, continue — do not redo work.
- If you cannot satisfy a blocking item without exceeding the PR's scope,
  STOP, write the reason to /tmp/wiwi-revise-abort.txt, and exit non-zero so
  a human can take over.

=== vivi's review (CHANGES_REQUESTED) ===
${REVIEW_BODY}

=== inline review comments ===
${INLINE:-(none)}

=== PR diff under review (truncated) ===
${DIFF}
EOF

# Wait for the model gateway before launching claude — a transient backend
# restart during the runner queue wait would otherwise kill the run.
LITELLM_WAIT="$(cd "$HERE/../lib" && pwd)/litellm-wait.sh"
# shellcheck source=../lib/litellm-wait.sh
source "$LITELLM_WAIT"
wait_for_litellm || exit $?

# Stream claude to both the workflow log and a file; retry on transient
# backend-down failures (same policy as run_wiwi.sh).
CLAUDE_RETRY_MAX="${CLAUDE_RETRY_MAX:-2}"
attempt=0
claude_exit=0
while true; do
    set +e
    stdbuf -oL claude --print \
      --allowed-tools Bash Read Write Edit Grep Glob \
      --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
      < "$PROMPT" 2>&1 | stdbuf -oL tee /tmp/wiwi-revise-run.log
    claude_exit=${PIPESTATUS[0]}
    set -e

    [ "$claude_exit" -eq 0 ] && break
    if ! litellm_appears_down; then break; fi
    if [ "$attempt" -ge "$CLAUDE_RETRY_MAX" ]; then
        echo "::error::wiwi revise: claude died $((attempt+1))× with gateway down; giving up" >&2
        break
    fi
    attempt=$((attempt + 1))
    echo "::warning::wiwi revise: claude exited $claude_exit, gateway down; waiting + retrying ($((attempt+1))/$((CLAUDE_RETRY_MAX+1)))" >&2
    wait_for_litellm || break
done

if [ "$claude_exit" != "0" ]; then
  echo "wiwi revise failed (claude exit=$claude_exit; see /tmp/wiwi-revise-run.log)" >&2
  gh pr comment "$PR" --body "🤖 wiwi could not complete the revision (see workflow log). Leaving vivi's review for a human."
  exit 1
fi

if [ -f /tmp/wiwi-revise-abort.txt ]; then
  gh pr comment "$PR" --body "🤖 wiwi paused the revision: $(cat /tmp/wiwi-revise-abort.txt)"
  exit 0
fi

# Defensive fallback: salvage any uncommitted edits so a crashed-before-commit
# run isn't lost to workspace cleanup.
if ! git diff --quiet || ! git diff --cached --quiet || \
   [ -n "$(git ls-files --others --exclude-standard)" ]; then
  echo "wiwi revise: auto-committing leftover edits (fallback)" >&2
  git add -A
  git commit -m "wiwi: revision for PR #${PR} (auto-commit fallback)" \
             -m "claude finished the revision without committing; this captures the working-tree state so the review feedback isn't lost. Reviewer: squash/reword as needed."
fi

# Nothing new to push? Then the revision produced no change — say so and stop
# rather than silently re-triggering an identical review. Compare HEAD against
# its upstream (origin/<branch>); equal ⇒ no new commit this pass.
upstream=$(git rev-parse '@{u}' 2>/dev/null || echo none)
if [ "$(git rev-parse HEAD)" = "$upstream" ]; then
  echo "wiwi revise: no new commits; nothing to push" >&2
  gh pr comment "$PR" --body "🤖 wiwi made no changes on this revision pass — vivi's feedback may need a human. Pausing the loop."
  exit 0
fi

git push
gh pr comment "$PR" --body "🤖 **wiwi** pushed a revision addressing vivi's review. CI + re-review run automatically."
