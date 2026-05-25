#!/usr/bin/env bash
# wiwi: dev agent. Branch off main, implement, ensure cargo build + tests
# green, open a DRAFT PR labelled `auto-agent`. Auto-merge gating happens
# downstream in pr-review.yml.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TEAM=$(grep -vE '^\s*(#|$)' "$HERE/TEAM" | tr '\n' ' ')
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
- Commit in logical chunks; sign-off line not required.

When done, write a brief summary to /tmp/wiwi-summary.md (Markdown) for
the PR body. End it with the literal line:

  Closes #${ISSUE_NUMBER}

Issue title: ${ISSUE_TITLE}
EOF

claude --print \
  --allowed-tools Bash Read Write Edit Grep Glob \
  --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
  < "$PROMPT" > /tmp/wiwi-run.log 2>&1 || {
    echo "wiwi run failed (see /tmp/wiwi-run.log)" >&2
    gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi could not complete this task. See workflow log."
    exit 1
}

if [ -f /tmp/wiwi-abort.txt ]; then
  gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi aborted: $(cat /tmp/wiwi-abort.txt)"
  exit 0
fi

# Sanity: must have produced commits.
if [ "$(git rev-list --count origin/main..HEAD)" = "0" ]; then
  gh issue comment "$ISSUE_NUMBER" --body "🤖 wiwi finished without any commit; nothing to PR."
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
