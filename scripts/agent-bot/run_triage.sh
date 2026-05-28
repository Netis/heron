#!/usr/bin/env bash
# Triage agent: read issue, decide do/skip/needs_info under STRICT gates.
# verdict=do  → add `agent:try` label (kicks off wiwi).
# else        → post a comment explaining gate failure; human can manually
#               add `agent:try` to force, or `agent:skip` to mute.
set -euo pipefail

PROMPT=$(mktemp)
OUT=$(mktemp)

cat > "$PROMPT" <<EOF
You are the **triage agent**. Decide if issue #${ISSUE_NUMBER} should be
auto-implemented by the dev agent **wiwi** running on this repo's
self-hosted runner with no human in the loop until PR review.

Verdict MUST be \`do\` only when ALL gates pass:

1. Issue has a concrete actionable description AND explicit acceptance
   criteria (you can list 2+ checkable assertions).
2. Estimated diff < 300 LOC across < 10 files.
3. Change is contained: console/, docs/, one crate, or one workflow —
   not cross-cutting architecture work.
4. No new runtime dependency, no new secret, no new external network
   call required.
5. The fix has a deterministic test (unit/integration/cargo check) that
   can be added in the same PR — not "needs manual QA".

If any gate fails, output verdict \`needs_info\` (gate 1 fails) or
\`skip\` (gates 2–5 fail). Be strict: when in doubt → \`needs_info\`.

Read the issue first (use \`gh issue view ${ISSUE_NUMBER}\`), inspect
referenced files, then emit exactly one JSON object on the last line of
your reply, no markdown fence:

{"verdict":"do|skip|needs_info","scope":"<short>","reason":"<≤200 chars>","files":["..."],"gates":{"1":true,"2":true,"3":true,"4":true,"5":true}}

Issue title: ${ISSUE_TITLE}
Author: ${ISSUE_AUTHOR}
EOF

# Wait for LiteLLM to be reachable before burning a runner slot
# inside claude --print. Caps at 30 min by default; configurable
# via MAX_LITELLM_WAIT_SECONDS. See scripts/lib/litellm-wait.sh.
LITELLM_WAIT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/litellm-wait.sh"
# shellcheck source=../lib/litellm-wait.sh
source "$LITELLM_WAIT"
wait_for_litellm || exit $?

# Run claude in print mode against our LiteLLM-style endpoint.
claude --print \
  --allowed-tools Bash Read Grep Glob WebFetch \
  --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
  < "$PROMPT" > "$OUT" 2> /tmp/triage-stderr.log || {
    echo "triage agent failed (see workflow log)" >&2
    cat /tmp/triage-stderr.log >&2
    exit 1
}

# Strict JSON parse: scan every line, keep ones that are valid JSON AND
# carry a `verdict` field, take the last. This rejects lines inside
# code fences, partial fragments, and lines that merely *mention*
# "verdict" in prose — only a parsable JSON object survives.
LAST=$(jq -Rrc 'fromjson? | select(.verdict)' "$OUT" 2>/dev/null | tail -1)
if [ -z "$LAST" ]; then
  echo "triage agent produced no parsable JSON verdict; aborting" >&2
  cat "$OUT" >&2
  exit 1
fi

VERDICT=$(echo "$LAST" | jq -r '.verdict')
REASON=$(echo  "$LAST" | jq -r '.reason')
SCOPE=$(echo   "$LAST" | jq -r '.scope')

# Defense-in-depth: require all 5 gates true for verdict=do.
if [ "$VERDICT" = "do" ]; then
  ALLPASS=$(echo "$LAST" | jq -r '[.gates."1",.gates."2",.gates."3",.gates."4",.gates."5"] | all')
  if [ "$ALLPASS" != "true" ]; then
    echo "verdict=do but not all gates true; downgrading to needs_info" >&2
    VERDICT=needs_info
    REASON="triage gates incomplete: $REASON"
  fi
fi

case "$VERDICT" in
  do)
    # Add the `agent:try` label using AGENT_GH_TOKEN (a PAT) rather
    # than the default GITHUB_TOKEN. GitHub deliberately suppresses
    # `labeled` events emitted by GITHUB_TOKEN to prevent recursive
    # workflow chains — meaning issue-implement.yml would never fire.
    # The PAT belongs to a real user, so its label edit fans out to
    # downstream workflows normally. If AGENT_GH_TOKEN is missing we
    # still label (with GITHUB_TOKEN) but the dev agent won't auto-start;
    # an operator can re-toggle the label by hand.
    if [ -n "${AGENT_GH_TOKEN:-}" ]; then
      GH_TOKEN="$AGENT_GH_TOKEN" gh issue edit "$ISSUE_NUMBER" --add-label "agent:try"
    else
      echo "warning: AGENT_GH_TOKEN unset; labeling under GITHUB_TOKEN — wiwi will NOT auto-start" >&2
      gh issue edit "$ISSUE_NUMBER" --add-label "agent:try"
    fi
    gh issue comment "$ISSUE_NUMBER" --body "🤖 Triage: **${VERDICT}** — scope: ${SCOPE}

${REASON}

Auto-labeled \`agent:try\`. **wiwi** will pick this up shortly."
    ;;
  needs_info|skip)
    gh issue comment "$ISSUE_NUMBER" --body "🤖 Triage: **${VERDICT}**

${REASON}

Manually add the \`agent:try\` label to override this verdict and run **wiwi** anyway, or \`agent:skip\` to mute future re-triage."
    ;;
  *)
    echo "unknown verdict: $VERDICT" >&2; exit 1 ;;
esac
