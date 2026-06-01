# mara — prod observer

Closes the incident loop: continuously polls the **production** heron and
files a deduplicated GitHub issue (with scrubbed context) when it detects a
failure, so a human no longer has to watch `/api/health` by hand. Filed
issues can be routed into the triage → wiwi loop by adding `agent:assess`.

Runs on a host **isolated from prod** (so it can still report if the prod
box dies), on a 5-minute systemd timer.

## What it detects

| Signature | Condition |
|---|---|
| `prod-heron-down` | `/api/health` unreachable / non-2xx |
| `prod-heron-parked` | health 2xx but `pipelines[].running == false` — capture silently stopped while health still reads "ready" |
| `prod-heron-healthbad` | 2xx but unparseable body |

A recent `panicked at` / `exited abnormally` / `FATAL` line from the log is
attached as **context** (not a trigger — avoids refiling on stale lines).
Every IPv4 and home path in the issue body is **masked** before it leaves the
host (same no-internal-infra rule the leakage linter enforces on the repo).

Dedup: a signature is not refiled within `MARA_DEDUP_SECS` (default 6h), and
mara skips filing if an open issue already names the signature.

## Install

```bash
sudo install -m 0755 scripts/agent-bot/mara.sh /usr/local/bin/mara.sh
sudo install -m 0644 scripts/agent-bot/mara.service scripts/agent-bot/mara.timer /etc/systemd/system/

# Config + token (chmod 600, NOT committed):
sudo install -d -m 0750 /etc/mara
sudo tee /etc/mara/env >/dev/null <<'ENV'
MARA_HEALTH_URL=http://<prod-host>:4500/api/health
MARA_LOG_HOST=<user>@<prod-host>
MARA_REPO=Netis/heron
# MARA_DRY_RUN=1   # log incidents without filing, until you're ready
ENV
sudo chmod 600 /etc/mara/env

# `gh` must be authenticated for the User= in mara.service (gh auth login),
# OR add GH_TOKEN=... to /etc/mara/env. (gh auth login needs a token with
# read:org; GH_TOKEN-based use only needs repo scope.)

# The issue labels must exist (gh won't auto-create them; mara falls back to
# filing without labels if they're missing, but you lose the categorisation):
gh label create mara --color 5319e7 --description "Filed by the mara prod observer"
gh label create incident --color d73a4a --description "Production incident"

sudo systemctl daemon-reload
sudo systemctl enable --now mara.timer
journalctl -u mara.service -f      # watch polls
```

## Config (env)

| Var | Default | Meaning |
|---|---|---|
| `MARA_HEALTH_URL` | — (required) | prod `/api/health` URL |
| `MARA_LOG_HOST` | — | ssh host for log context (optional) |
| `MARA_LOG_PATH` | `/tmp/heron.log` | log path on that host |
| `MARA_REPO` | `Netis/heron` | repo to file into |
| `MARA_LABELS` | `mara,incident` | issue labels |
| `MARA_DEDUP_SECS` | `21600` | refile suppression window |
| `MARA_DRY_RUN` | `0` | `1` → print instead of filing (no token needed) |

## Status

v1 = health-based DOWN/PARKED detection + scrubbed context + dedup. Not yet:
a `/api/ops/heartbeat` push side, metric-regression detection, or the `/ops`
dashboard — see the quality-infra plan.
