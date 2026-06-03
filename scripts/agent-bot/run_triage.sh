#!/usr/bin/env bash
# Triage agent: read issue, decide do/skip/needs_info under STRICT gates.
# Fires automatically on every newly opened issue (TRIGGER_KIND=opened) and
# on a manual `agent:assess` re-trigger (TRIGGER_KIND=assess).
# verdict=do  → add `agent:try` label (kicks off wiwi).
# else        → post a comment with a detailed, reporter-facing breakdown of
#               which gates failed and what to add; human can manually add
#               `agent:try` to force, or `agent:skip` to mute.
set -euo pipefail

# ---------------------------------------------------------------------------
# Auto-path guards. On the auto path (TRIGGER_KIND=opened) we skip two classes
# of issue so they never get auto-routed into the autonomous dev agent:
#   - prod incidents filed by mara (incident/mara labels) — an operator routes
#     these in deliberately via `agent:assess`.
#   - issues already in the pipeline or muted (agent:try / agent:skip /
#     auto-agent) — avoids duplicate triage and re-trigger loops.
# A manual `agent:assess` (TRIGGER_KIND=assess) bypasses these guards entirely:
# the human asked for triage explicitly.
# ---------------------------------------------------------------------------
if [ "${TRIGGER_KIND:-opened}" = "opened" ]; then
  LABELS=$(gh issue view "$ISSUE_NUMBER" --json labels --jq '[.labels[].name] | join(",")' 2>/dev/null || echo "")
  case ",$LABELS," in
    *,incident,*|*,mara,*)
      echo "auto-triage skipped: prod-incident issue #$ISSUE_NUMBER; add agent:assess to route it manually"
      exit 0 ;;
  esac
  case ",$LABELS," in
    *,agent:try,*|*,agent:skip,*|*,auto-agent,*)
      echo "auto-triage skipped: issue #$ISSUE_NUMBER is already queued/muted/has-PR"
      exit 0 ;;
  esac
fi

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

When the verdict is NOT \`do\`, you MUST fill \`detail\` with a thorough,
reporter-facing explanation (this becomes the GitHub comment they read):
  - List EACH failed gate by number and state concretely WHY it failed for
    THIS issue, referencing the specific files/areas you inspected.
  - For \`needs_info\`, spell out exactly what the reporter must add
    (acceptance criteria as checkable assertions, a repro, a narrowed scope)
    so it can pass on a manual \`agent:assess\` re-triage.
  - For \`skip\`, explain why it is out of wiwi's safe envelope and what a
    human would need to do instead.
Keep \`reason\` a ≤200-char one-line summary; put the depth in \`detail\`.

Read the issue first (use \`gh issue view ${ISSUE_NUMBER}\`), inspect
referenced files, then emit exactly ONE JSON object as the LAST line of your
reply — a single line, no markdown fence, with any newlines inside string
values escaped as \\n:

{"verdict":"do|skip|needs_info","scope":"<short>","reason":"<≤200-char summary>","detail":"<required when verdict!=do: markdown enumerating each failed gate + what the reporter must add; use \\n for line breaks>","files":["..."],"gates":{"1":true,"2":true,"3":true,"4":true,"5":true}}

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
# Wrap in a retry loop that catches the specific case "claude
# crashed because LiteLLM died mid-stream". Up to CLAUDE_RETRY_MAX
# retries (default 2 → 3 attempts total). Triage is idempotent —
# the input is just the issue body — so restart from scratch is
# safe.
CLAUDE_RETRY_MAX="${CLAUDE_RETRY_MAX:-2}"
attempt=0
claude_rc=0
while true; do
    set +e
    claude --print \
      --allowed-tools Bash Read Grep Glob WebFetch \
      --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
      < "$PROMPT" > "$OUT" 2> /tmp/triage-stderr.log
    claude_rc=$?
    set -e

    [ "$claude_rc" -eq 0 ] && break

    # claude failed. Was it because LiteLLM is down right now?
    if ! litellm_appears_down; then
        # LiteLLM is up; the failure is something else (rate limit,
        # token budget, claude bug). Don't retry.
        break
    fi

    if [ "$attempt" -ge "$CLAUDE_RETRY_MAX" ]; then
        echo "::error::triage claude died $((attempt+1)) times with LiteLLM down each time; giving up" >&2
        break
    fi

    attempt=$((attempt + 1))
    echo "::warning::triage claude exited $claude_rc with LiteLLM down; waiting + retrying (attempt $((attempt+1))/$((CLAUDE_RETRY_MAX+1)))" >&2
    wait_for_litellm || break
done

if [ "$claude_rc" -ne 0 ]; then
    echo "triage agent failed (see workflow log)" >&2
    cat /tmp/triage-stderr.log >&2
    exit 1
fi

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
DETAIL=$(echo  "$LAST" | jq -r '.detail // ""')

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
    # Prefer the detailed, per-gate breakdown; fall back to the short reason
    # (e.g. if a do→needs_info downgrade left detail empty).
    BODY_DETAIL="${DETAIL:-$REASON}"
    gh issue comment "$ISSUE_NUMBER" --body "🤖 Triage: **${VERDICT}** — ${SCOPE}

${BODY_DETAIL}

---
Manually add the \`agent:try\` label to override this verdict and run **wiwi** anyway, or \`agent:skip\` to mute future re-triage."
    ;;
  *)
    echo "unknown verdict: $VERDICT" >&2; exit 1 ;;
esac
