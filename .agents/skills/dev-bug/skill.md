---
version: 1.1.0
description: "Unified bug lifecycle skill. Usage: /dev-bug <action> [args]"
---

# Bug Lifecycle Management

Unified bug record, review, and fix skill. Maintains **two parallel stores** per bug:

- **Human store** (`{bug.dir}`) — detailed postmortem for developer learning
- **AI-compact store** (`{bug.ai_dir}`) — one-line-per-field summary for token-efficient automated scanning

Both stores are updated by `record` / `fix`; `review` reads only the AI-compact store for speed.

## Parameters
- **Arguments**: $ARGUMENTS — `<action> [args]`

## Actions

| Action | Usage | Description |
|--------|-------|-------------|
| `record` | `/dev-bug record <context>` | Record bug from adversarial review or manual description |
| `review` | `/dev-bug review [--module M] [index\|all]` | Scan code for historical bug patterns |
| `fix` | `/dev-bug fix <context>` | record + fix + regression test |

Parse the first word of `$ARGUMENTS` as action, dispatch to the corresponding section.

---

## Step 0 (shared): Load Project Configuration

Every action starts by reading `project.yaml` from the scope root:

```yaml
bug:
  dir: docs/bug                    # human store (canonical, default: docs/bug)
  readme: docs/bug/README.md       # human index (default: {bug.dir}/README.md)
  ai_dir: context/bug              # AI-compact store (default: context/bug)
  ai_readme: context/bug/README.md # AI index (default: {bug.ai_dir}/README.md)

language:
  docs: English                    # Both AI and human files follow this
```

**Defaults when a field is missing:**
- `bug.dir` → `docs/bug`
- `bug.readme` → `{bug.dir}/README.md`
- `bug.ai_dir` → `context/bug`
- `bug.ai_readme` → `{bug.ai_dir}/README.md`
- `language.docs` → `English` (alias: `Chinese` ≡ `中文`)

**Never hardcode the paths** `docs/bug` or `context/bug` in the body of any action — always resolve through the config variables above. The only fixed thing is their default values.

**Output language**: Both the AI context file and the human-readable file are written in `language.docs`. File names, `location` paths, and grep `pattern` fields remain as-is — never translate code identifiers or regex patterns.

---

## Action: record

Record a bug postmortem from adversarial review output or manual description.

### Step 1: Parse Input

Extract from context:
1. **module** — infer owning submodule from file paths (workspace mode)
2. **location** — `file:line`
3. **cause** — one-line root cause description
4. **fix** — one-line fix description
5. **lesson** — one-line lesson learned principle
6. **pattern** — grep regex pattern (used by `review` action to scan)
7. **title** — short title
8. **slug** — kebab-case filename suffix

If information is insufficient, use `AskUserQuestion` to fill in gaps. **Never invent a regex pattern** — if the user can't articulate one, record the bug with `pattern: TODO` and surface a note in the report (review will skip TODO-pattern entries).

### Step 2: Determine Index

Take the **maximum index across both stores** (human and AI), then +1. This guarantees uniqueness even when the two stores are out of sync (e.g. one was backfilled manually but not the other).

```bash
human_max=$(ls {scope}/{bug.dir}/[0-9][0-9][0-9][0-9]-*.md 2>/dev/null \
  | sed -E 's|.*/([0-9]{4}).*|\1|' | sort -r | head -1)
ai_max=$(ls {scope}/{bug.ai_dir}/[0-9][0-9][0-9][0-9].md 2>/dev/null \
  | sed -E 's|.*/([0-9]{4})\.md|\1|' | sort -r | head -1)
next=$(printf "%04d" $((10#${human_max:-0000} > 10#${ai_max:-0000} ? 10#${human_max:-0000} + 1 : 10#${ai_max:-0000} + 1)))
```

If both stores are empty, start at `0001`. Format: `NNNN` (4-digit zero-padded).

**Consistency check**: if `human_max != ai_max` and both are non-empty, surface a warning ("human store at 0019, AI store at 0015 — reconcile manually"). Continue with the computed `next` regardless.

### Step 3: Write AI Compact File

Create `{scope}/{bug.ai_dir}/{index}.md`:

```markdown
# {index} {title}
- location: `{file}:{line}`
- cause: {one-line description}
- fix: {one-line description}
- lesson: {one-line principle}
- pattern: `{grep regex}`
- ref: [{index}-{date}-{slug}.md](../../{bug.dir}/{index}-{date}-{slug}.md)
```

### Step 4: Write Human-Readable File

Create `{scope}/{bug.dir}/{index}-{date}-{slug}.md`:

```markdown
# Bug: {Title}

**Date**: {YYYY-MM-DD}
**Location**: `{file_path}:{line_number}`
**Status**: Fixed

## Symptoms

{Description of what was observed}

## Root Cause

{Root cause analysis}

{Optional: code snippet}

## Fix

{Fix description}

{Optional: code snippet}

## Lesson Learned

**{Short principle}** - {Detailed explanation}

## Related
- AI Context: [{bug.ai_dir}/{index}.md](../../{bug.ai_dir}/{index}.md)
```

### Step 5: Update AI Index

Append a row to `{scope}/{bug.ai_readme}` (create if missing):

```markdown
# Bug Index (AI-compact)

One line per bug — used by `/dev-bug review` for regression scanning.

Format: `- {index} | {location} | {lesson} | \`{pattern}\``

- {index} | {location} | {lesson} | `{pattern}`
```

If the README exists but lacks the leading comment block, leave it alone and just append the new row.

### Step 6: Update Human Index

Append a row to the table in `{scope}/{bug.readme}` (create if missing):

```markdown
# Bug Log

Postmortems for bugs found and fixed. Each entry documents symptoms, root cause, fix, and lesson learned.

| Index | Date | Location | Issue | Lesson Learned |
|-------|------|----------|-------|----------------|
| {index} | {date} | `{location}` | [{title}]({index}-{date}-{slug}.md) | {lesson} |
```

### Step 7: Update Lessons Learned (Optional)

If the lesson has broad applicability, consider adding it to `{scope}/AGENTS.md` Lessons Learned section.

### Step 8: Report

```
Bug recorded: {index}

Files:
- {scope}/{bug.dir}/{index}-{date}-{slug}.md (human)
- {scope}/{bug.ai_dir}/{index}.md (AI compact)
- {scope}/{bug.readme} (human index updated)
- {scope}/{bug.ai_readme} (AI index updated)

Lesson: {lesson}
```

---

## Action: review

Scan the codebase for code matching historical bug patterns. Reads **only** from the AI-compact store for token efficiency.

### Step 1: Parse Arguments

| Input | Scope | AI store path |
|-------|-------|---------------|
| (no `--module`) | Parent repo | `{bug.ai_dir}` |
| `--module <mod>` | Submodule | `<module>/{bug.ai_dir}` (per submodule's own `project.yaml`) |

Module resolution rules (workspace mode):
1. Read `project.yaml` → `workspace.file` (e.g. `project-workspace.yaml`) for submodule list
2. Match `<mod>`: exact > suffix (`core` → `nexus-core`) > prefix
3. If no workspace file, fall back to `git submodule status`
4. On ambiguity, list candidates and ask the user

Bug index argument (after `--module`):
- `all` or empty: review all recorded bugs
- Comma-separated indices (e.g. `0003,0005`): review specified bugs
- Single index (e.g. `0003`): review that bug

### Step 2: Pre-check

Test whether the AI store is populated:

```bash
test -s {scope}/{bug.ai_readme}
```

- **If missing/empty** but `{scope}/{bug.readme}` exists and has rows → report:
  ```
  Human bug history exists at {bug.dir} but AI-compact store at {bug.ai_dir} is missing.
  Backfill AI-compact records from the human store before running review.
  ```
  Exit.
- **If both missing** → report "no bug records" and exit.
- **If AI store present** → proceed.

### Step 3: Load Bug Patterns

Parse `{scope}/{bug.ai_readme}` line-by-line:

```
- 0001 | src/pipeline.rs | Never silently discard errors | `unwrap_or_default|unwrap_or\(`
```

For each line, extract `index`, `location`, `lesson`, `pattern`. **Skip entries where pattern is `TODO` or empty** — collect their indices into a `skipped_patterns` list for the final report.

If specific indices were given, load only matching lines.

### Step 4: Derive Scan Roots (adaptive)

Scan roots come from the bug entries themselves — **never hardcoded**.

For each bug being reviewed, extract the top-level directory from its `location` field:

```
src/pipeline.rs                 → src/
scripts/routers/shared/dev.sh   → scripts/
.Codex/skills/decode/skill.md  → .Codex/skills/decode/
data/chatspace/.../20.json      → data/chatspace/    (rare; still honored)
```

Deduplicate the collected roots. These become the adaptive scan surface — the review investigates exactly the directories where recorded bugs historically lived, which naturally adapts to each target project's layout without requiring `bug.scan_roots` config.

**Optional override**: if `project.yaml` defines `bug.scan_roots: [src/, scripts/, ...]`, use that list instead of the derived set.

### Step 5: Execute Scan

For each bug's pattern, grep within its derived scan root(s):

```bash
grep -rn -E "{pattern}" {scope}/{scan_root} \
  --exclude-dir=target --exclude-dir=dist --exclude-dir=node_modules \
  --exclude-dir=__pycache__ --exclude-dir=.venv --exclude-dir=.git \
  --exclude-dir="{bug.dir}" --exclude-dir="{bug.ai_dir}"
```

Excluding the bug stores themselves prevents false positives where a bug report literally contains the regex it warns about.

### Step 6: Analyze Results

For each match:
1. Read surrounding code context
2. Determine if it matches the bug pattern
3. Classify:
   - **Issue**: Confirmed bug (matches documented pattern)
   - **Warning**: Potential problem (similar pattern, needs review)
   - **OK**: False positive (pattern present but correctly handled)

### Step 7: Report

```markdown
# Bug Review Report

**Date**: {YYYY-MM-DD}
**Scope**: {module or parent}
**Bugs Checked**: {index list}
**Scan Roots (adaptive)**: {derived root list}
**Skipped (pattern=TODO)**: {skipped_patterns or "none"}

## Issues Found

### Bug #{index}: {lesson}
**Pattern**: `{pattern}`
**Scan root**: `{root}`

| File | Line | Status | Notes |
|------|------|--------|-------|
| {path} | {line} | Issue/Warning/OK | {explanation} |

## Summary
- **Issues**: {count}
- **Warnings**: {count}
- **OK**: {count}
- **Skipped**: {skipped count} (patterns marked TODO — see `/dev-bug record` to fill in)
```

### Step 8: Fix Suggestions (Optional)

If issues are found, ask the user:
- Fix all issues
- Review one by one
- Report only

---

## Action: fix

Full bug lifecycle: record -> fix -> regression test. For standalone use (outside dev-story).

### Step 1: Parse Input

Same as `record` — accepts adversarial review output or user description.

### Step 2: Record Bug

Internally run the `record` flow (Steps 1-8 above, under shared Step 0), obtaining the bug index and full report.

### Step 3: Implement Fix

1. Read the root cause and fix description from the bug report
2. Locate the problem code (using the location field)
3. Implement the fix
4. Ensure the fix meets project code standards (run lint/format)

### Step 4: Add Regression Test

1. Create a targeted regression test based on the bug's pattern
2. Test naming: `test_{bug_slug}` or project test naming convention
3. The test must:
   - Fail before the fix (validates test effectiveness)
   - Pass after the fix
4. Run the test suite to confirm no regressions

### Step 5: Verify Fix

Run `review` for this bug index to confirm the pattern no longer matches new code.

### Step 6: Report

```
Bug #{index} fixed:

Record:
- {scope}/{bug.ai_dir}/{index}.md
- {scope}/{bug.dir}/{index}-{date}-{slug}.md

Fix:
- {modified files list}

Tests:
- {new test files/functions}

Review: pattern no longer matches
```

---

## Notes

- AI context files are designed for token efficiency, used by automated scanning and gate workflows
- Human-readable files maintain detailed format for developer learning and reference
- Both file types are cross-linked via `ref` / `Related` sections
- **Both stores must stay in sync.** If they drift (e.g. a human bug was added manually without the AI mirror), reconcile by manually creating the missing AI-compact record
- **Patterns are authoritative.** `review` trusts `pattern` regexes as recorded; mark `TODO` if uncertain rather than guessing
- **Scan roots are adaptive.** Review never hardcodes `src/` — it derives scan surfaces from each bug's recorded `location` field, which naturally adjusts to whatever directory layout the target project uses
