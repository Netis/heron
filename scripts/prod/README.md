# Production deploy

Gated promotion of a staging-soaked build to **production** (the live heron on
the prod host). This is the tail of the quality chain — everything upstream
(CI → staging deploy → soak) has already validated the binary; this stage puts
it on prod behind a human approval and an automatic health gate.

## Chain

```
ci(main) → deploy-staging → staging-soak (tara)
   │  all green
   ▼
deploy-prod.yml   (workflow_run on staging-soak success, OR manual dispatch)
   │
   ├─ runs on the `prod-deploy` self-hosted runner ON the prod host
   ├─ environment: production  ──►  PAUSES for a required-reviewer approval
   └─ deploy-prod.sh <sha>
          ├─ git fetch + checkout the validated commit
          ├─ snapshot the current binary (for rollback)
          ├─ cargo build --release --features console   (warm-cache incremental)
          ├─ smoke `heron --version`
          ├─ sudo systemctl restart heron.service
          ├─ health gate: status=ready AND pipeline running (≤120s)
          └─ rollback to the snapshot + restart if the gate fails
```

The build happens BEFORE the service is touched, so a build failure leaves prod
untouched. `heron.service` grants capture caps via `AmbientCapabilities`, so no
`setcap` is ever needed.

## Why a manual approval (not fully automatic)

Prod captures real LLM-proxy traffic — a bigger blast radius than the staging
VM. The synthetic-corpus soak (L7) can't see real-traffic shapes/volume, so
until a shadow-canary stage (L8) validates the build against *live* traffic, a
human approves the prod flip. Once the canary lands, the approval can relax to
automatic (canary-green → promote).

## The `prod-deploy` runner

A self-hosted runner on the prod host (systemd service, runs as the deploy
user). It is **only** used by `deploy-prod.yml` — its label is not shared with
PR CI, so PR/fork code never executes on the prod host. Same trust model as the
`staging-deploy` runner, but on the prod host because the deploy is local
(build + `systemctl`), with no SSH/VM hop.

## Config (no machine-specifics in source)

- Repo Variable `HERON_PROD_REPO_DIR` — the persistent heron checkout on the
  prod host (warm cargo cache). The workflow passes it to `deploy-prod.sh`;
  the script requires it (no hardcoded path).
- `deploy-prod.sh` env knobs: `HERON_PROD_SERVICE` (default `heron.service`),
  `HERON_PROD_PORT` (4500), `HEALTH_TIMEOUT_SECS` (120), `CARGO_BIN`.

Unlike staging (which ships `scripts/staging/{config.toml,heron.service}`), the
**prod config and systemd unit are provisioned on the host, not in this repo** —
they hold host-specific paths/ports and live at `~/.config/heron/config.toml`
and `/etc/systemd/system/heron.service` on the prod box. `deploy-prod.sh` only
swaps the binary + restarts the existing unit; it never templates either file.
The unit must grant capture caps via `AmbientCapabilities=CAP_NET_RAW
CAP_NET_ADMIN` (so a rebuild needs no `setcap`) and set `Restart=on-failure`.

> Health gate: requires `status=ready`; if a capture pipeline is configured it
> must be `running`. An empty `pipelines` array (API-only / maintenance) is
> treated as healthy rather than failing the deploy.

## Manual deploy / dispatch

```bash
# On the prod host, deploy a specific commit (or origin/main):
HERON_PROD_REPO_DIR=<checkout> scripts/prod/deploy-prod.sh <git-sha>

# Or trigger the gated workflow (still requires the environment approval):
gh workflow run deploy-prod.yml --repo Netis/heron -f sha=<git-sha>
```
