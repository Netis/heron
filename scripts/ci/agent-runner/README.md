# The `heron` agent runner

A containerised self-hosted runner for the workflows that run an agent against
this repo:

| Workflow | What it does on this runner |
|---|---|
| `pr-review` | reviews the diff of a same-repo PR, posts a review, merges or dispatches a revision |
| `pr-revise` | applies review feedback to the PR branch |
| `issue-triage` | assesses an issue against the `.olympus.json` gates |
| `issue-implement` | implements a gated issue — runs `just build all && just test all` |
| `pr-review-probe` | asserts this runner has everything the above need |

## Why a container, and why not the prod host

These jobs check out PR-branch code and build it. The prod host runs the live
capture service and the `prod-deploy` runner; the repo's trust model keeps that
label unshared with PR CI precisely so PR code never executes there. A container
gives the isolation without a hypervisor, on a host that already runs Docker.

## Build

```bash
docker build -t heron-agent-runner scripts/ci/agent-runner

# On a network where the crates.io sparse index crawls, point cargo elsewhere.
# Deliberately a build arg and not a committed default — the right mirror is a
# property of where the image runs, not of this repository.
docker build -t heron-agent-runner \
  --build-arg CARGO_MIRROR_URL=<sparse-index-url> scripts/ci/agent-runner
```

Add `--network host` if the host reaches github.com by a path the default
bridge does not — a Tailscale route or exit node, most commonly. The symptom is
specific and easy to misread: `apt` and ordinary mirrors work from the bridge,
`objects.githubusercontent.com` answers, and only `github.com`/`api.github.com`
time out, because those are the hostnames the tailnet route covers. See
*Networking* below.

The image carries: rust stable (+rustfmt, clippy), bun, node + the Claude Code
CLI, `gh`, `just`, python3, `envsubst`, and the build-time C dependencies the
workspace links against (`libpcap-dev` is required even for crates that never
capture — `h-storage` pulls `h-capture` in transitively).

## Run

The runner's registration lives in a mounted volume, so a restart re-attaches
the same runner rather than registering a second one and leaving an offline
ghost in the repo's runner list.

```bash
docker volume create heron-agent-runner-state

docker run -d --name heron-agent-runner --restart unless-stopped \
  --network host \
  -v heron-agent-runner-state:/home/runner/actions-runner \
  -e RUNNER_URL=https://github.com/Netis/heron \
  -e RUNNER_TOKEN="$(gh api -X POST repos/Netis/heron/actions/runners/registration-token --jq .token)" \
  -e RUNNER_LABELS=heron \
  heron-agent-runner
```

`RUNNER_TOKEN` is only read on the first start and expires in about an hour; on
later starts the mounted `.runner` is used and the variable is ignored.

## Networking

The runner has to reach `github.com` and `api.github.com` continuously — that
is how it takes jobs — plus the model gateway named by the `LITELLM_BASE_URL`
secret.

On a host whose route to GitHub runs over a tailnet rather than the default
gateway, the docker bridge does not inherit it: Tailscale installs its route in
its own table and does not forward bridge traffic into `tailscale0`, so
container traffic falls back to the default route and hangs. Two ways out:

* **`--network host`** on both build and run. Nothing to configure, survives
  reboots, and it is the one flag the run command above already carries — drop
  it on a host with ordinary egress. Without it on a tailnet host the runner
  registers, reports online, and then never picks up a job. The cost is that the
  container shares the host's network namespace, so it can reach services bound
  to the host's loopback. On a host that also runs production, that includes
  them — aglake's search API in particular is unauthenticated by design, with
  the bind address as its only access control. Weigh that against what the jobs
  are: same-repo PR branches built by an agent you configured.
* **Route the bridge through the tailnet** and keep the container's own network
  namespace — a `MASQUERADE` for the bridge subnet out of `tailscale0` plus the
  matching `FORWARD` accepts, made persistent. Better isolation, at the price of
  firewall rules on the host that outlive this container.

If the gateway is on the container host, note that under bridge networking a
`127.0.0.1` base URL resolves to the container, not the host, and fails as
"gateway unreachable"; publish it on a routable address or use `--add-host`.

## Verify

```bash
gh workflow run pr-review-probe.yml --repo Netis/heron
```

The probe checks the CLI, `gh` auth, python + `envsubst`, that the gateway
accepts the key, and that a round trip through the agent returns a token from
the model. It is the definition of "this runner is ready" — run it after every
rebuild.

## Retire

```bash
docker rm -f heron-agent-runner
gh api -X DELETE repos/Netis/heron/actions/runners/<id>
docker volume rm heron-agent-runner-state
```

Deleting the container without deleting the registration leaves an offline
runner that jobs will still queue against.
