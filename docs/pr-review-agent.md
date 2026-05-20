# PR review agent

A headless code-review agent that fires after CI passes and before
a human picks up the PR. The goal isn't to replace human review — it's
to surface the obvious-in-hindsight stuff (schema mirror drift,
body-column scans, missing queryKey deps, classifier rules sensitive
to window width, etc.) so the human reviewer arrives at a PR with the
"easy 80%" already triaged.

## Architecture

```
GitHub                                  wuneng VM tokenscope-ci
┌──────────────┐  ci passes (workflow_run)  ┌─────────────────────────┐
│  PR opened   │ ─────────────────────────► │ self-hosted GH runner   │
│  PR sync     │                            │  ┌───────────────────┐  │
│              │                            │  │ pr-review.yml     │  │
│              │                            │  │  ↓                │  │
│              │                            │  │ run_review.sh ────┼──┼─► LiteLLM :4200
│              │                            │  │   claude -p       │  │     ↓
│              │                            │  │   read-only tools │  │   SGLang GLM-5 :9000
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
  template, pre-flights LiteLLM, runs `claude -p` with the read-only
  tool allowlist + 1800 s outer timeout, writes the model's stdout to
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

The `tokenscope` self-hosted runner on wuneng's `tokenscope-ci` VM
needs:

1. **Claude Code CLI** installed and on `$PATH`:
   ```
   npm i -g @anthropic-ai/claude-code
   ```
2. **GitHub CLI** authenticated:
   ```
   gh auth login         # one-time, as the `tokenscope-review-bot` account
   ```
3. **Python 3** (default `python3` is fine — `post_review.py` uses
   stdlib only).
4. **Network path** to LiteLLM at `172.16.103.81:4200` (the VM is on
   wuneng's libvirt bridge, so this works out of the box).

The workflow exports `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` /
`ANTHROPIC_MODEL` per-job. LiteLLM rewrites
`claude-3-5-sonnet-20241022` onto GLM-5.

## Cost / latency

GLM-5 runs on-prem (GPUs 4-7 of wuneng, served by SGLang). No
per-request cost; the constraint is GPU minutes.

| PR size | Files | Input tokens | Output tokens | Wall clock |
|---|---|---|---|---|
| Small | 1–2 | 20–40 K | 3–6 K | 2–3 min |
| Medium | 5–10 | 60–150 K | 8–15 K | 5–8 min |
| Large | 30+ | 250–500 K | 15–25 K | 15–25 min |

Concurrency cap: GH Actions `concurrency` is per-PR, but the runner
itself is single-tenant (one job at a time). Multiple PRs serialize
naturally. If we hit a "many PRs at once" pattern we can raise the
runner's job-slot count, but two concurrent reviews is the ceiling
before we start crowding training jobs on the same box.

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
| LiteLLM down | Pre-flight curl fails, `run_review.sh` exits 2 | `post_review.py` posts a brief "agent failed" comment with link to workflow log; PR is not blocked |
| Agent loops | `timeout 1800` kills the agent | Same: failure comment, no block |
| GLM-5 returns garbage / no `### Summary` | `run_review.sh` appends a warning to the output | `post_review.py` still posts it as COMMENT — the agent's broken output is visible, which is signal |
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
