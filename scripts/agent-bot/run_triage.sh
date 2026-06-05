#!/usr/bin/env bash
# Triage agent: investigate EVERY newly-filed issue for real (read the code,
# reproduce where feasible), then reply in a warm, first-person maintainer
# voice — no matter the verdict.
#
# The strict 5-gate verdict decides ONLY whether the autonomous dev agent
# (wiwi) may implement the issue UNATTENDED — it does NOT decide whether the
# issue is valid or whether it deserves a careful, friendly answer. It always
# does.
#   verdict=do  → reproduce/confirm, add `agent:try` (kicks off wiwi), and post
#                 a warm "I've reproduced it, it's tracked, I'm on it" reply.
#   else        → post an equally warm, equally investigated reply that thanks
#                 the reporter, explains in plain language why it can't be
#                 auto-queued, and asks concrete follow-ups / offers a
#                 workaround. A non-`do` verdict is NEVER a brush-off.
#
# Fires automatically on every newly opened issue (TRIGGER_KIND=opened) and on
# a manual `agent:assess` re-trigger (TRIGGER_KIND=assess).
set -euo pipefail

# ---------------------------------------------------------------------------
# compose_comment_body — build the exact GitHub comment body for a verdict.
#   $1 reply      : the maintainer-voice markdown the triage agent wrote
#                   (may be empty — a fallback is substituted)
#   $2 verdict    : do | needs_info | skip
#   $3 downgraded : "1" when a do→needs_info safety downgrade fired. The agent
#                   wrote `reply` for a `do` verdict (it promises the work is
#                   queued), so it would mislead now — replace it with an
#                   honest fallback.
# Emits the body on stdout. Pure: no gh / claude / network, so tests drive it
# directly (see tests/test_triage.py).
# ---------------------------------------------------------------------------
compose_comment_body() {
  local reply="$1" verdict="$2" downgraded="${3:-0}"

  if [ "$downgraded" = "1" ] || [ -z "$reply" ]; then
    reply="Thanks so much for taking the time to file this — I really appreciate it. 🙏

I had a look into it, but before I pick it up I want to be sure I'd be fixing exactly the right thing. Could you add a couple of concrete, checkable acceptance criteria — and a quick way to reproduce it, if you have one? With those in hand I'll gladly take another pass and get it moving."
  fi

  printf '%s\n' "$reply"

  # For anything we are NOT auto-queuing, surface the manual overrides — but
  # tucked into a collapsed block so they never intrude on the human reply.
  if [ "$verdict" = "needs_info" ] || [ "$verdict" = "skip" ]; then
    cat <<'FOOT'

<details><summary>Maintainer controls</summary>

This isn't auto-queued for the dev agent. A maintainer can add **`agent:try`** to have it attempted anyway, or **`agent:skip`** to mute re-triage. After editing the issue, re-add **`agent:assess`** to run triage again.
</details>
FOOT
  fi

  # Invisible breadcrumb (no rendered output) so re-triage / tooling can tell a
  # triage-authored comment from a human one without polluting the voice.
  printf '\n<!-- heron-triage:%s -->\n' "$verdict"
}

# Sourced by tests with TRIAGE_LIB_ONLY=1: load the helpers above and stop
# before the live triage flow (which needs gh / claude / the network).
if [ "${TRIAGE_LIB_ONLY:-}" = "1" ]; then
  return 0 2>/dev/null || exit 0
fi

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

# NOTE: unquoted heredoc — \${ISSUE_*} interpolate. Every literal backtick is
# escaped as \` and the JSON's literal newline marker is written as \\n.
cat > "$PROMPT" <<EOF
You are a **Heron maintainer** triaging a freshly-filed issue (#${ISSUE_NUMBER}).
You have two jobs, in this order.

──────────────────────────────────────────────────────────────────────────
JOB A — INVESTIGATE FOR REAL (always, regardless of the verdict below)
──────────────────────────────────────────────────────────────────────────
Before you decide or write anything, genuinely look into it:
  - Read the whole issue: \`gh issue view ${ISSUE_NUMBER}\`.
  - Inspect the code/area it concerns (Read / Grep / Glob) and, when it's
    feasible in a couple of minutes, **reproduce it** — run the relevant
    command, trace the code path, replay a fixture pcap, whatever gives you
    real evidence. Time-box it: a focused code read plus a quick check. Never
    kick off a long build.
  - Form an honest opinion: is the report accurate? what's the real cause, or
    the real gap? This investigation is REQUIRED on every issue.

──────────────────────────────────────────────────────────────────────────
JOB B — DECIDE if the autonomous dev agent can implement it UNATTENDED
──────────────────────────────────────────────────────────────────────────
Verdict \`do\` ONLY when ALL of these hold:
  1. Concrete, actionable description AND explicit acceptance criteria (you can
     list 2+ objectively checkable assertions).
  2. Estimated diff < 300 LOC across < 10 files.
  3. Contained: console/, docs/, one crate, or one workflow — not cross-cutting
     architecture work.
  4. No new runtime dependency, no new secret, no new external network call.
  5. Ships with a deterministic test (unit / integration / \`cargo check\`) in
     the same PR — not "needs manual QA".
If gate 1 fails → \`needs_info\`. If gates 2–5 fail → \`skip\`. When in doubt,
be strict → \`needs_info\`.

CRUCIAL FRAMING: this gate decides ONLY whether an unattended agent can safely
take the issue. It does NOT decide whether the issue is worthwhile or whether
it earns a thoughtful answer. **Every** reporter gets a warm, genuine,
investigated reply. A \`needs_info\` or \`skip\` is never a rejection of the
idea or of the person — plenty of excellent issues are simply bigger than an
unattended agent should attempt.

──────────────────────────────────────────────────────────────────────────
WRITE THE REPLY (the \`reply\` field)
──────────────────────────────────────────────────────────────────────────
\`reply\` is the COMPLETE GitHub comment, in your own warm, first-person human
voice — a maintainer who is honestly glad this was filed. Requirements:
  - NO robotic header ("Triage: do"), NO gate numbers, NO checklist. Translate
    any gate concern into plain, friendly language.
  - Open by thanking them for THIS specific report.
  - Show your work: say what you actually looked at / tried to reproduce and
    what you found — concretely, naming the real behavior or code area.
  - Then, by verdict:
    • \`do\`: confirm you've reproduced/understood it, that it's now tracked and
      queued to be worked on, and that you'll keep this issue posted. Be warm
      but don't over-promise a fix time.
    • \`needs_info\`: explain plainly what you'd need to be confident you'd fix
      the RIGHT thing, and ask 1–3 specific, concrete questions. Offer a
      workaround or your best guess if you have one. Make following up feel
      easy and welcome.
    • \`skip\`: make clear the idea is welcome — it's just larger or more
      cross-cutting than is safe to hand an unattended agent, so it's better as
      a human-guided change. Suggest how to break it down or what the next step
      is, and offer to help scope it.
  - Close warmly.

HYGIENE (hard rule): write only about the PUBLIC repository. NEVER mention any
internal infrastructure — no private IPs, internal hostnames, CI/runner machine
names, internal model/proxy names, or internal agent code-names. Speak as
"I" / "we" / "the team". This comment is public.

──────────────────────────────────────────────────────────────────────────
OUTPUT
──────────────────────────────────────────────────────────────────────────
Emit exactly ONE JSON object as the LAST line of your reply — a single line, no
markdown fence, with every newline inside a string value escaped as \\n:

{"verdict":"do|skip|needs_info","scope":"<short>","reason":"<=200-char log summary>","reply":"<the full maintainer-voice GitHub comment markdown; \\n for line breaks>","files":["..."],"gates":{"1":true,"2":true,"3":true,"4":true,"5":true}}

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
REPLY=$(echo   "$LAST" | jq -r '.reply // ""')

# Defense-in-depth: require all 5 gates true for verdict=do. A model that
# returns do with a failing gate gets downgraded — and its `do`-flavored reply
# (which promises the work is queued) is dropped in favor of an honest fallback
# inside compose_comment_body.
DOWNGRADED=0
if [ "$VERDICT" = "do" ]; then
  ALLPASS=$(echo "$LAST" | jq -r '[.gates."1",.gates."2",.gates."3",.gates."4",.gates."5"] | all')
  if [ "$ALLPASS" != "true" ]; then
    echo "verdict=do but not all gates true; downgrading to needs_info" >&2
    VERDICT=needs_info
    REASON="triage gates incomplete: $REASON"
    DOWNGRADED=1
  fi
fi

echo "triage verdict=$VERDICT scope=$SCOPE reason=$REASON" >&2

TMP_BODY=$(mktemp)
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
    compose_comment_body "$REPLY" do 0 > "$TMP_BODY"
    gh issue comment "$ISSUE_NUMBER" --body-file "$TMP_BODY"
    ;;
  needs_info|skip)
    compose_comment_body "$REPLY" "$VERDICT" "$DOWNGRADED" > "$TMP_BODY"
    gh issue comment "$ISSUE_NUMBER" --body-file "$TMP_BODY"
    ;;
  *)
    echo "unknown verdict: $VERDICT" >&2; rm -f "$TMP_BODY"; exit 1 ;;
esac
rm -f "$TMP_BODY"
