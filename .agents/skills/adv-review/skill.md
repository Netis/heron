---
version: 0.1.0
description: "Adversarial review pipeline: /codex:adversarial-review --wait → /dev-bug fix per finding. Usage: /adv-review [--base|--ref <ref>] [<focus>]"
---

# Advanced (Adversarial) Review

Pipeline skill that chains **Codex adversarial review** → **dev-bug fix** for every finding.

One command turns "codex found 6 issues" into "6 dual-store bug records, 6 code fixes, 6 regression tests" — sequentially, with each fix verified before the next begins.

## Pipeline

```
/adv-review [--base <ref>] [<focus>]
  │
  ├─ Step 1: Parse args (flags + NL focus)
  ├─ Step 2: Derive scope hints from NL focus (grep for terms)
  ├─ Step 3: Run /codex:adversarial-review --wait [--base X] [<focus + hints>]
  ├─ Step 4: Fetch structured findings via /codex:result <job-id>
  ├─ Step 5: If verdict=approve → report clean, exit
  ├─ Step 6: For each finding (sequential):
  │            /dev-bug fix <finding-context>
  │              → records dual-store bug
  │              → implements fix
  │              → adds regression test
  └─ Step 7: Final report (indices, fixes, tests)
```

## Parameters

- **Arguments**: `$ARGUMENTS` — `[--base|--ref <ref>] [<natural-language focus>]`
  - `--base <ref>` / `--ref <ref>` (aliases) — git ref to diff against; forwarded to codex as `--base <ref>`
  - Everything else → natural-language focus text (passed to codex verbatim + used as scope hint)

## Why `--wait`, not `--background`

This skill is a **pipeline**: codex findings feed directly into `dev-bug fix` in the same turn. `--background` detaches the codex run and returns control immediately, which would break the chain. `--wait` blocks until codex completes so we have findings in-hand to iterate.

This matches how `dev-story` (finai-nexus) invokes codex at its adversarial-review gate: `--wait --scope branch`.

---

## Step 0: Load Project Configuration

Read `project.yaml` from the scope root:

```yaml
git:
  default_branch: main    # used when --base/--ref omitted with scope=branch

language:
  docs: English           # forwarded to dev-bug fix via its own config
```

**Defaults when missing:**
- `git.default_branch` → `main`

No adv-review-specific config exists. The skill piggybacks on dev-bug's configuration (bug dir, ai_dir, language) — dev-bug fix reads those itself when invoked.

---

## Step 1: Parse Arguments

Split `$ARGUMENTS` into:
1. **`base_ref`** — value of `--base` or `--ref` if present (mutually exclusive; if both given, error out)
2. **`focus`** — all remaining non-flag tokens, joined as a single free-text string

Examples:

| Input | base_ref | focus |
|-------|----------|-------|
| `/adv-review` | *(none)* | *(none)* |
| `/adv-review --base main` | `main` | *(none)* |
| `/adv-review auth race conditions` | *(none)* | `auth race conditions` |
| `/adv-review --ref develop rollback safety` | `develop` | `rollback safety` |

---

## Step 2: Derive Scope Hints from NL Focus

If `focus` is non-empty, convert it into a list of candidate files codex should weight heavily. This is preprocessing on *our* side — codex itself only accepts git-based scopes.

### Algorithm

1. Tokenize `focus` into keywords: split on whitespace, lowercase, drop stopwords (`the`, `a`, `of`, `in`, `on`, `and`, `or`, `for`, `to`, `is`, `with`).
2. For each remaining keyword, grep for it across tracked source files:
   ```bash
   git grep -l -i "<keyword>" -- \
     ':!**/node_modules/**' ':!**/dist/**' ':!**/target/**' \
     ':!**/.venv/**' ':!**/*.lock' ':!**/*.md'
   ```
3. For each file, count how many distinct keywords matched.
4. Keep files where **at least half** of the non-stopword keywords matched (minimum 1). Rationale: a file mentioning one of five terms is probably unrelated noise; a file mentioning three of five is probably on-topic.
5. Cap the list at **10 files** (sorted by match count, then by path). Codex's context is finite; more than 10 files dilutes the focus.

If grep yields zero files, skip the hint — pass only the raw focus text.

### Built focus string

```
<original focus>

Relevant files: <path1>, <path2>, ...
```

This goes into codex as the trailing focus text. Codex's adversarial-review prompt (`adversarial-review.md:34`) weights user focus heavily, so file paths inside the focus text act as soft scope hints.

---

## Step 3: Invoke Codex Adversarial Review

Always with `--wait`. Build the command:

```
/codex:adversarial-review --wait [--base <base_ref>] [<built focus string>]
```

Flag handling:

| Condition | Forwarded flags |
|-----------|-----------------|
| `base_ref` present | `--base <base_ref>` (no `--scope`, codex infers branch scope from base) |
| `base_ref` absent | *(none)* — codex defaults to `--scope auto` (working-tree if dirty, else branch) |

Run the slash command. Because it's `--wait`, the call blocks until codex finishes.

### On failure

- **Codex errors / non-zero exit**: surface the stderr and abort. Do not try to record bugs from partial output.
- **Verdict `approve`**: report "adversarial review clean — no findings to fix" and exit with success. Do not invoke dev-bug at all.
- **Verdict `needs-attention` with empty findings[]**: treat as codex confusion — surface the raw output and ask the user whether to continue.

---

## Step 4: Fetch Structured Findings

The foreground slash command returns rendered text to stdout, but the **structured JSON** (with precise line ranges and confidence scores) is stored in the job payload.

### Preferred path: `/codex:result <job-id>`

1. After `--wait` returns, call `/codex:status` to get the just-finished job ID.
2. Call `/codex:result <id>` — returns the full payload including `result.findings[]`.
3. Parse findings from the JSON payload:

```json
{
  "verdict": "needs-attention",
  "summary": "...",
  "findings": [
    {
      "file": "src/auth/middleware.rs",
      "line_start": 123,
      "line_end": 145,
      "confidence": 0.85,
      "title": "...",
      "body": "...",
      "recommendation": "..."
    }
  ]
}
```

### Fallback: scrape rendered output

If `/codex:result` is unavailable for any reason, parse findings from the rendered `--wait` output. The rendering from `renderReviewResult` (codex-companion.mjs:417) produces a consistent format with file paths and line ranges — scrape each finding as a block.

---

## Step 5: Present Summary (Read-Only)

Print a findings table before any fixes start so the user sees what's about to happen:

```
Adversarial Review Findings
===========================
Verdict: needs-attention
Scope: {base_ref or "working-tree"}
Focus: {focus or "(none)"}

| # | File | Lines | Confidence | Title |
|---|------|-------|------------|-------|
| 1 | src/auth/mw.rs | 123-145 | 0.85 | Missing tenant check on refresh path |
| 2 | src/pipeline.rs | 88-102 | 0.72 | Unhandled partial failure in retry loop |
| ...
```

This is **informational only** — per the user's decision, adv-review auto-fixes all findings sequentially. No selection prompt.

---

## Step 6: Fix Loop (Sequential)

For each finding in `findings[]` order:

### Step 6a: Build context string

Format the finding into a context string that `/dev-bug fix` can parse. `dev-bug fix` Step 1 extracts `location`, `cause`, `fix`, `lesson`, `pattern`, `title`, `slug` from the context — give it enough structure to do that cleanly.

```
Source: /codex:adversarial-review (confidence: {confidence})

Title: {title}
Location: {file}:{line_start}-{line_end}

Issue:
{body}

Recommendation:
{recommendation}
```

### Step 6b: Invoke `/dev-bug fix`

```
/dev-bug fix <context string from 6a>
```

`dev-bug fix` then runs its full flow (skill.md:319):
1. Parse context → extract fields (ask user only if truly insufficient)
2. Internally run `record` → creates both AI-compact and human postmortem files with new index
3. Implement the fix at the specified location
4. Add a regression test (fails before fix, passes after)
5. Run `review` for the new index to confirm the pattern no longer matches
6. Report

### Step 6c: Verify before proceeding

Before starting the next finding:
- If the previous fix modified files that later findings reference, re-read those files — line numbers may have shifted.
- If the project has a fast quality check (`just quality all` or equivalent), run it. A broken fix shouldn't poison the queue.
- If a fix fails (code doesn't compile, test doesn't pass), **stop the loop**, report the partial state, and surface the failure to the user. Do not silently skip.

### Step 6d: Line-shift handling

When finding #N references `file.rs:200-210` but finding #1 already modified lines 50-80 of the same file, the recorded line range for finding #N may now point to the wrong code. Before calling `/dev-bug fix` for finding #N:

1. Re-open the file.
2. Search for a distinctive token from the finding's `body` (function name, variable, error message).
3. If found, update the location in the context string to the new line range.
4. If not found, note "location may have shifted — verify" in the context and let `dev-bug fix` ask the user.

---

## Step 7: Final Report

```
Adversarial Review Complete
===========================
Codex findings: {N}
Bugs recorded:  {N} (indices: {i1}, {i2}, ...)
Fixes applied:  {N}
Tests added:    {N}
Review pass:    {N of N patterns no longer match}

Files touched:
- {path1}
- {path2}
...

Next steps:
- Run full test suite: {project's test command}
- Consider /dev-commit to commit the batch as one normalized change
```

If the loop was interrupted (Step 6c failure), report partial progress and surface the failing finding explicitly.

---

## Usage Examples

```bash
# Review the working tree (codex auto scope), no focus
/adv-review

# Review the current branch vs main
/adv-review --base main

# --ref is an alias for --base
/adv-review --ref develop

# Focus with natural language (scope hints derived from grep)
/adv-review auth middleware race conditions

# Combine flag + focus
/adv-review --base main rollback safety and retry loops
```

## Notes

- **This skill is destructive** — it records bugs AND modifies code AND adds tests. Run it on a clean branch or worktree, not on work you can't afford to lose.
- **`--wait` can be slow.** Codex adversarial-review on a large diff may take several minutes. The skill will appear to hang during Step 3 — that's expected.
- **Sequential, not parallel.** The fix loop must be sequential because later fixes may reference lines moved by earlier fixes. Do not try to parallelize Step 6.
- **`/dev-bug fix` already adds regression tests** (dev-bug/skill.md:339) — adv-review does not duplicate that responsibility. Any change to test-generation policy belongs in `dev-bug`, not here.
- **Scope-hint derivation is a heuristic, not gospel.** The default (Step 2) works reasonably well but is not tuned for any specific project. If you find it over- or under-selecting files, that's the right place to refine.
