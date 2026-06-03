# PR review agent

A headless code-review agent that fires after CI passes and before
a human picks up the PR. The goal isn't to replace human review — it's
to surface the obvious-in-hindsight stuff (schema mirror drift,
body-column scans, missing queryKey deps, classifier rules sensitive
to window width, etc.) so the human reviewer arrives at a PR with the
"easy 80%" already triaged.

## Architecture

```
GitHub                                  self-hosted runner
┌──────────────┐  ci passes (workflow_run)  ┌─────────────────────────┐
│  PR opened   │ ─────────────────────────► │ self-hosted GH runner   │
│  PR sync     │                            │  ┌───────────────────┐  │
│              │                            │  │ pr-review.yml     │  │
│              │                            │  │  ↓                │  │
│              │                            │  │ run_review.sh ────┼──┼─► model gateway
│              │                            │  │   claude -p       │  │     ↓
│              │                            │  │   read-only tools │  │   model backend
│              │                            │  │  ↓                │  │
│              │  gh pr review              │  │ post_review.py    │  │
│              │ ◄──────────────────────────┤  └───────────────────┘  │
└──────────────┘                            └─────────────────────────┘
```

## Components

* `.github/workflows/pr-review.yml`
  Trigger: `workflow_run` on the `ci` workflow's `completed` event,
  gated on `conclusion == 'success'` and `event == 'pull_request'`.
  Also accepts `workflow_dispatch` for manual re-runs (`gh workflow
  run pr-review.yml -f pr_number=27`).
* `scripts/pr-review/run_review.sh`
  Substitutes `PR_NUMBER` / `HEAD_SHA` / `BASE_REF` into the prompt
  template, pre-flights the model gateway, runs `claude -p` with the read-only
  tool allowlist + 7200 s outer timeout, writes the model's stdout to
  `/tmp/pr-review-${N}-out.md`.
* `scripts/pr-review/prompt.md`
  Instructional prompt. Encodes repo facts the agent has to know
  before reading the diff (crate map, schema mirror rules, repo's
  history of footguns) and the strict output format the parser
  expects.
* `scripts/pr-review/allowed_tools.txt`
  Explicit allowlist — `Read`, `Grep`, `Glob`, and a few inspection
  Bash patterns. No `Edit`, no `Write`, no unrestricted `Bash(*)`.
* `scripts/pr-review/post_review.py`
  Reads the agent's output, picks the review event from the section
  population (`Blocking` → REQUEST_CHANGES, `Suggestions`/`Questions`
  → COMMENT, none → APPROVE), and hands it to `gh pr review`. Falls
  back to a plain `gh pr comment` if the bot can't review the PR
  (e.g. it authored it).

## Trigger sequence

```
1. PR opened / synchronize
2. `ci` workflow runs (cargo test, console build, …)
3. `ci` completes with success
4. `workflow_run` fires `pr-review` (this workflow)
5. pr-review checks out the PR head, runs the agent
6. agent posts a single PR review (APPROVE / COMMENT / REQUEST_CHANGES)
```

If CI fails, the review agent never runs. That's intentional — there's
no value paying for a review on a PR that won't build.

## Manual re-run

```
gh workflow run pr-review.yml -f pr_number=27
```

The `concurrency` block ensures a manual re-trigger cancels any
in-flight review of the same PR — no duplicate comments.

## Self-hosted runner expectations

The `heron` self-hosted runner needs:

1. **Claude Code CLI** installed and on `$PATH`:
   ```
   sudo npm i -g @anthropic-ai/claude-code
   ```
2. **GitHub CLI** — comes free with the actions runner via the
   `GH_TOKEN` the workflow exports.
3. **Python 3** — `post_review.py` uses stdlib only.
4. **Network path + key** to a model gateway that maps the requested
   model alias onto a locally-served backend. Configured entirely via
   repo secrets so this stays portable:
   * `LITELLM_BASE_URL` — gateway origin (e.g. `http://host:port`)
   * `LITELLM_API_KEY` — gateway master key
   * `LITELLM_NO_PROXY` — comma-separated host list to bypass any
     ambient `HTTP_PROXY` set on the runner
5. **No ambient HTTP proxy interference**: if the runner has
   `HTTP_PROXY` / `HTTPS_PROXY` set (cloud-init defaults often
   do), `LITELLM_NO_PROXY` should include the gateway host so curl
   from the agent bypasses it.

## Auto-merge for trusted authors

When the AI's verdict is **APPROVE** and the PR's author is in
`AUTO_MERGE_AUTHORS` (a maintainer allowlist supplied via the
workflow env; empty by default), `post_review.py` follows up the review with
`gh pr merge --admin --squash --delete-branch`. The repo doesn't
have native `--auto` enabled, so we squash inline.

Rationale: an APPROVE from the AI on a low-stakes change by the
project maintainer is enough signal to land. Anyone else's PRs
still wait for a human reviewer — the AI's review is informational,
not a merge gate.

If the merge fails (merge conflict, branch protection surprise,
…) it's logged but the workflow stays green — the review is
already posted, an operator can finish the merge by hand.

## Auto-revise on CHANGES_REQUESTED (wiwi follow-up)

The mirror image of auto-merge. When vivi's verdict is
**REQUEST_CHANGES** on a **wiwi** PR (label `auto-agent`),
`scripts/agent-bot/revise_dispatch.sh` (run from pr-review.yml's tail)
dispatches the **`pr-revise`** workflow. That workflow checks out the PR
head branch, runs `scripts/agent-bot/run_revise.sh` to feed vivi's
blocking feedback + the diff back to the dev agent, and — once the build
is green — commits and pushes. The push re-triggers `ci` → `pr-review`,
so vivi re-reviews the revised PR. On the next APPROVE the existing
auto-merge gate lands it; no human in the loop for the happy path.

```
vivi REQUEST_CHANGES (auto-agent PR)
  → revise_dispatch.sh → gh workflow run pr-revise
  → run_revise.sh: address review · build green · commit · push
  → ci → pr-review (vivi re-review) ──┐
        APPROVE → auto-merge          │
        REQUEST_CHANGES again ────────┘  (loop, bounded)
```

Two implementation constraints worth knowing:

- **PAT, not GITHUB_TOKEN.** The dispatch uses `AGENT_GH_TOKEN`. A
  `gh workflow run` (or a review) issued with the default `GITHUB_TOKEN`
  is dropped by GitHub's anti-recursion rule, and the dispatched run must
  itself be able to push and re-trigger `ci`/`pr-review`.
- **Bounded loop.** `revise_dispatch.sh` counts vivi's
  CHANGES_REQUESTED reviews on the PR; at `MAX_REVISE_ROUNDS` (default 3)
  it stops dispatching, adds a `needs-human` label, and comments — so a
  wiwi ↔ vivi disagreement can't spin forever. Remove the label and
  re-run `pr-revise` to resume.

The agent pool (`heron`) runs `pr-revise` because `run_revise.sh` drives
the model gateway, same as the reviewer and wiwi.

## Cost / latency

The model runs on a locally-served backend. No per-request cost; the
constraint is local compute capacity.

| PR size | Files | Input tokens | Output tokens | Wall clock |
|---|---|---|---|---|
| Small | 1–2 | 20–40 K | 3–6 K | 2–3 min |
| Medium | 5–10 | 60–150 K | 8–15 K | 5–8 min |
| Large | 30+ | 250–500 K | 15–25 K | 15–25 min |

Concurrency cap: GH Actions `concurrency` is per-PR, but the runner
itself is single-tenant (one job at a time). Multiple PRs serialize
naturally. If we hit a "many PRs at once" pattern we can raise the
runner's job-slot count, but two concurrent reviews is the ceiling
before we start crowding other workloads on the runner host.

## Tuning the prompt

The "Things this repo has been bitten by" section in `prompt.md` is
the most valuable knob. Every time the agent misses a class of bug
the human reviewer catches, add a one-line entry. Every time the
agent flags a non-issue often enough to be annoying, refine or
remove the corresponding entry.

The prompt is intentionally repo-specific. A vanilla "review this
diff" prompt produces generic style notes; the value of a per-repo
agent is encoded prior knowledge about the repo's own historic
footguns.

## Failure modes

| Failure | What happens | Mitigation |
|---|---|---|
| Model gateway down | Pre-flight curl fails, `run_review.sh` exits 2 | `post_review.py` posts a brief "agent failed" comment with link to workflow log; PR is not blocked |
| Agent loops | `timeout 7200` kills the agent | Same: failure comment, no block |
| Model returns garbage / no `### Summary` | `run_review.sh` appends a warning to the output | `post_review.py` still posts it as COMMENT — the agent's broken output is visible, which is signal |
| Bot can't `--approve` its own PR | `gh pr review` rc != 0 | `post_review.py` falls back to `gh pr comment` |
| Schema mirror miss inside the agent | Agent under-reports | Add the missed signature to `prompt.md` § "Things this repo has been bitten by" — encode the lesson |

## Phasing

* **Phase 1 (this PR)**: scaffolding. `workflow_run` trigger gated on
  CI success. Manual `workflow_dispatch` for re-runs. Test on a few
  real PRs.
* **Phase 2**: collect a calibration set of past PRs + human review
  comments. Tune the prompt to converge with reviewer judgment. Add
  a nightly self-check workflow that re-runs against a canonical
  test PR and alerts on schema drift.
* **Phase 3**: structured inline comments (`gh pr review
  --comment line=...`) once we trust the line numbers in the agent's
  output. Today the agent emits `file:line` references in markdown
  and reviewers click through manually — fine for v1.
