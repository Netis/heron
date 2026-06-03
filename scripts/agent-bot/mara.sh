#!/usr/bin/env bash
# mara — prod observer. One poll of the production heron: detect failure
# conditions and file a deduplicated GitHub issue (with context) so the
# triage -> wiwi loop can pick it up. Closes the incident loop that a human
# otherwise has to watch by hand.
#
# Runs on a host ISOLATED from prod (a separate box from the one it watches),
# driven by a systemd timer (mara.timer). Each invocation is one poll; state
# (for dedup) persists in $MARA_STATE_DIR.
#
# Detected conditions (health-based, current-state — no stale-log ambiguity):
#   DOWN    : /api/health unreachable / non-2xx / unparseable
#   PARKED  : health 2xx but pipeline running=false (capturing has stopped —
#             the silent failure mode where /api/health still looks "ready")
# A recent panic / "exited abnormally" line from the log is attached as
# CONTEXT to the issue (not a separate trigger, to avoid refiling on old log
# lines).
#
# Config (env; nothing internal hardcoded — the systemd unit supplies these):
#   MARA_HEALTH_URL   required, e.g. http://<prod-host>:4500/api/health
#   MARA_LOG_HOST     optional ssh host for log context (e.g. user@host)
#   MARA_LOG_PATH     log path on MARA_LOG_HOST           (default /tmp/heron.log)
#   MARA_REPO         GitHub repo                          (default Netis/heron)
#   MARA_LABELS       issue labels (comma-sep)             (default mara,incident)
#   MARA_STATE_DIR    dedup state dir                      (default $HOME/.mara)
#   MARA_DEDUP_SECS   don't refile same signature within   (default 21600 = 6h)
#   MARA_CONFIRM_POLLS    polls that must ALL fail to file (default 3, 1=off)
#   MARA_CONFIRM_DELAY_SECS  seconds between confirm polls  (default 10)
#   MARA_DRY_RUN      "1" → print the issue instead of filing (needs no token)
#   GH_TOKEN          PAT for `gh` (from the unit's EnvironmentFile) unless dry-run
#
# A single failed poll is NOT filed on its own: a prod deploy restarts heron
# (health is briefly down, and the pipeline reads running=false until capture
# resumes — observed ~10 s), and a one-off network hiccup looks identical. mara
# re-polls and only files when EVERY poll fails, so a deploy/restart blip never
# opens a phantom incident. (For an airtight deploy guard, pair with a
# maintenance-window sentinel — A reduces but can't fully eliminate the window
# for an unusually long restart.)
set -uo pipefail

HEALTH_URL="${MARA_HEALTH_URL:?set MARA_HEALTH_URL}"
LOG_HOST="${MARA_LOG_HOST:-}"
LOG_PATH="${MARA_LOG_PATH:-/tmp/heron.log}"
REPO="${MARA_REPO:-Netis/heron}"
LABELS="${MARA_LABELS:-mara,incident}"
STATE_DIR="${MARA_STATE_DIR:-$HOME/.mara}"
DEDUP_SECS="${MARA_DEDUP_SECS:-21600}"
CONFIRM_POLLS="${MARA_CONFIRM_POLLS:-3}"
CONFIRM_DELAY="${MARA_CONFIRM_DELAY_SECS:-10}"
DRY_RUN="${MARA_DRY_RUN:-0}"
GH_BIN="${GH_BIN:-$(command -v gh || echo "$HOME/bin/gh")}"

mkdir -p "$STATE_DIR"
SEEN="$STATE_DIR/seen"   # lines: "<signature>\t<epoch>"
touch "$SEEN"
now=$(date +%s)

# Scrub internal-infra identity before anything goes into a (public) issue.
# The whole issue body is piped through this, so every rule added here covers
# every field at once. Masks, in order:
#   1. IPv4 dotted-quads          (the heron DEBUG log is full of RFC1918 +
#                                  docker-bridge addresses)
#   2. home-directory paths       (/home/<user>, /Users/<user>)
#   3. URL authorities            (scheme://HOST:PORT → scheme://<host>) — so an
#                                  internal hostname in MARA_HEALTH_URL or a
#                                  logged URL never lands in the issue. IPv4
#                                  hosts are already masked by rule 1.
#   4. ssh-style user@host tokens (user@host → <user>@<host>)
# Same PR-hygiene rule the check-leakage linter enforces on the repo; rules
# 3/4 close the hostname gap that rules 1/2 (IP/path only) left open.
scrub() {
  # Group the home/Users prefix so the literal pattern never spells out
  # "/home/<char>" or "/Users/<char>" (which would trip the leakage linter on
  # this very file). Username class excludes '/' so the path tail is kept.
  sed -E -e 's/\b[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\b/<ip>/g' \
         -e 's#(/home|/Users)/[A-Za-z0-9._-]+#\1/<user>#g' \
         -e 's#(https?://)[^/[:space:]]+#\1<host>#g' \
         -e 's/\b[A-Za-z0-9._-]+@[A-Za-z0-9._-]+/<user>@<host>/g'
}

# ---- probe health -------------------------------------------------------
# One health probe. Sets globals: code, json, signature, summary.
# Empty `signature` = healthy.
probe_once() {
  local hbody running
  hbody="$(curl -s -m 8 -w '\n%{http_code}' "$HEALTH_URL" 2>/dev/null || printf '\n000')"
  code="${hbody##*$'\n'}"
  json="${hbody%$'\n'*}"
  signature=""
  summary=""
  if [ "$code" != "200" ]; then
    signature="prod-heron-down"
    summary="heron /api/health unreachable or non-200 (HTTP ${code})"
  else
    running="$(printf '%s' "$json" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin)["data"]; print(str(d["pipelines"][0]["running"]).lower())
except Exception: print("parseerror")' 2>/dev/null)"
    if [ "$running" = "false" ]; then
      signature="prod-heron-parked"
      summary="heron is up (health=ready) but the pipeline has stopped (running=false) — capture silently halted"
    elif [ "$running" = "parseerror" ]; then
      signature="prod-heron-healthbad"
      summary="heron /api/health returned 200 but an unparseable body"
    fi
  fi
}

probe_once
if [ -z "$signature" ]; then
  echo "mara: prod heron OK (HTTP $code, pipeline running) — no incident"
  exit 0
fi

# ---- confirm the failure is SUSTAINED, not a transient blip -------------
# Re-poll up to MARA_CONFIRM_POLLS times; if ANY confirmation comes back
# healthy, the first hit was transient (deploy/restart window, one-off network
# hiccup) → do NOT file. Only a failure on EVERY poll is reported, using the
# freshest snapshot. CONFIRM_POLLS=1 disables this (file on first failure).
first_sig="$signature"
n=1
while [ "$n" -lt "$CONFIRM_POLLS" ]; do
  sleep "$CONFIRM_DELAY"
  n=$((n + 1))
  probe_once
  if [ -z "$signature" ]; then
    echo "mara: '$first_sig' cleared on confirm poll $n/$CONFIRM_POLLS — transient (likely a deploy/restart blip), not filing"
    exit 0
  fi
  echo "mara: failure persists on confirm poll $n/$CONFIRM_POLLS (signature=$signature)" >&2
done
# Confirmed sustained. Refresh `now` so dedup/SEEN reflect confirm time, not the
# first probe (the confirm polls added a few seconds).
now=$(date +%s)

# ---- dedup: skip if filed within the window ----------------------------
last="$(awk -F'\t' -v s="$signature" '$1==s{print $2}' "$SEEN" | tail -1)"
if [ -n "$last" ] && [ $(( now - last )) -lt "$DEDUP_SECS" ]; then
  echo "mara: '$signature' already reported $(( (now-last)/60 ))m ago (< dedup window) — skipping"
  exit 0
fi

# ---- gather log context (best-effort) ----------------------------------
logctx="(no log host configured)"
if [ -n "$LOG_HOST" ]; then
  logctx="$(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=6 \
      "$LOG_HOST" "grep -iE 'panicked at|exited abnormally|FATAL' '$LOG_PATH' | tail -8; echo '---- tail ----'; tail -20 '$LOG_PATH'" 2>&1 \
    || echo '(log host unreachable — the whole box may be down)')"
fi

stamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
title="[mara] prod incident: ${signature}"
body="$(cat <<EOF
🤖 **mara** detected a production heron incident.

- **Signature**: \`${signature}\`
- **What**: ${summary}
- **Health URL**: ${HEALTH_URL}
- **HTTP**: ${code}
- **Observed (UTC)**: ${stamp}
- **Confirmed**: failed ${CONFIRM_POLLS}/${CONFIRM_POLLS} consecutive polls (~${CONFIRM_DELAY}s apart) — not a transient/deploy blip

### Health response
\`\`\`json
${json}
\`\`\`

### Log context
\`\`\`
${logctx}
\`\`\`

---
Filed automatically by mara (prod observer). Dedup window: $(( DEDUP_SECS/3600 ))h.
Add \`agent:assess\` to route this to the triage → wiwi loop once confirmed actionable.
EOF
)"

# Mask internal IPs / home paths in the whole body before it leaves the host.
body="$(printf '%s' "$body" | scrub)"

if [ "$DRY_RUN" = "1" ]; then
  echo "==== mara DRY RUN — would file ===="
  echo "title: $title"
  echo "labels: $LABELS"
  echo "$body"
  exit 0
fi

# ---- dedup against open issues (survives state-file loss) ---------------
existing="$("$GH_BIN" issue list --repo "$REPO" --state open --search "in:title ${signature}" --json number --jq '.[0].number' 2>/dev/null || true)"
if [ -n "$existing" ]; then
  echo "mara: open issue #$existing already tracks '$signature' — recording + skipping"
  printf '%s\t%s\n' "$signature" "$now" >> "$SEEN"
  exit 0
fi

url="$("$GH_BIN" issue create --repo "$REPO" --title "$title" --label "$LABELS" --body "$body" 2>&1)"
if ! printf '%s' "$url" | grep -q 'github.com/'; then
  # gh won't auto-create a missing label and fails the whole call — a missing
  # label must not lose the incident, so retry without labels.
  echo "mara: create with labels failed ($url); retrying without labels" >&2
  url="$("$GH_BIN" issue create --repo "$REPO" --title "$title" --body "$body" 2>&1)"
fi
if printf '%s' "$url" | grep -q 'github.com/'; then
  echo "mara: filed $url"; printf '%s\t%s\n' "$signature" "$now" >> "$SEEN"
else
  echo "mara: gh issue create FAILED: $url" >&2; exit 1
fi
