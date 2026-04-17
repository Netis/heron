---
version: 3.1.1
tier: submodule-aware
description: "Normalized commit with natural language intent. Usage: /dev-commit [intent]"
---

# Normalized Commit

Create well-structured commits with natural language intent parsing.
Submodule-aware: detects dirty submodules, commits them first, then the parent.

## Step 0: Parse User Intent

Analyze `{{$ARGUMENTS}}` for three intent signals:

### 1. Staging Intent

**RULE: Only empty input means "stage all." Any non-empty input is a scoping signal.**

| Input                                  | Action                                            |
| -------------------------------------- | ------------------------------------------------- |
| (empty)                                | Stage all unstaged changes (`git add -A`)         |
| `staged`, `only staged`, `just staged` | Commit only what's already staged, don't add more |
| `staged and X`, `staged + X`           | Keep staged + add matching unstaged               |
| **Anything else**                      | **Selective staging — find matching files**       |

**How to resolve file scope from input:**

1. Run `git status` to get all changed/untracked files
2. Match input against changed files using these rules (in priority order):

| Input Type              | Example                         | Match Strategy                                    |
| ----------------------- | ------------------------------- | ------------------------------------------------- |
| Submodule name          | any name or alias from project.yaml | Scope to that submodule only (see Submodule Intent)|
| `@path` reference       | `@src/lib/workflow.py`          | Exact file + related files                        |
| Explicit path           | `src/lib/workflow.py`           | Exact file + related files                        |
| Module/filename keyword | `workflow`, `auth`              | Search changed files for name match               |
| Directory keyword       | `prompts`, `scripts`            | All changed files under that directory             |

3. "related" means include directly related files (test file, tightly-coupled imports)
4. Stage ONLY matched files with `git add <file1> <file2> ...`
5. **Never `git add -A` when input is non-empty**

### 1a. Submodule Intent

When the input references a submodule name (full or alias), scope the commit to that submodule.

**Resolve the submodule list at runtime — NEVER hardcode names.**

Lookup order:

1. **`project.yaml` → `submodules:`** (preferred, per-project source of truth)

   ```yaml
   submodules:
     - name: backend
       aliases: [be]
     - name: frontend
       aliases: [fe, ui]
   ```

2. **`project-workspace.yaml` → `submodules:`** (workspace-type projects)

3. **`.gitmodules`** (fallback — full submodule names only, no aliases):

   ```bash
   git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '{print $2}'
   ```

If none of these exist or list no submodules, skip Step 1a entirely — the project has no submodules and submodule intent cannot apply.

**Matching rule:** A token from the input matches a submodule if it equals the submodule's `name` or any of its `aliases`. Matching is case-insensitive and considers only the first whitespace-separated tokens of the input (before descriptive words).

**Combined with descriptive intent:**

| Input pattern                  | Action                                                   |
| ------------------------------ | -------------------------------------------------------- |
| `<submodule>`                  | Commit all changes in that submodule only                |
| `<submodule> <words...>`       | Commit that submodule; remaining words = message hint    |
| `<submodule-a> and <submodule-b>` | Commit each in order, then update parent pointers     |
| `<submodule> amend`            | Amend last commit in that submodule                      |

The remaining words after submodule matching serve as **commit message hints** — use them to inform the type, scope, and subject of the commit message.

**Example** (for a project declaring `backend` with alias `be` in its project.yaml):
`be parser fixes` → resolves to `backend` → commit inside backend with message like `fix(parser): ...`.

### 2. Amend Intent

| Keywords                                   | Action                                            |
| ------------------------------------------ | ------------------------------------------------- |
| (none)                                     | New commit                                        |
| `amend`, `add to last`, `include in last`  | Amend last commit, update message                 |
| `amend keep`, `amend no message`           | Amend last commit, keep original message          |
| `fix message`, `reword`, `improve message` | Reword last commit message only (no file changes) |

### 3. Intent Examples

| User Input               | Staging                   | Amend    | Result                                    |
| ------------------------ | ------------------------- | -------- | ----------------------------------------- |
| (empty)                  | All                       | No       | Submodules first, then parent             |
| `staged`                 | Staged only               | No       | Commit staged, new commit                 |
| `<submodule>`            | All in that submodule     | No       | Commit submodule only, update parent ptr  |
| `<submodule> <hint>`     | All in that submodule     | No       | Commit with message hint from words       |
| `<sub-a> and <sub-b>`    | Both submodules           | No       | Commit each, then update parent ptrs      |
| `workflow related`       | `workflow.py` + test file | No       | Selective, new commit                     |
| `@src/lib/auth.py fixes` | `auth.py` + related       | No       | Selective, new commit                     |
| `prompts`                | `prompts/**` changes      | No       | Selective, new commit                     |
| `staged and tools`       | Staged + `tools/**`       | No       | Combined, new commit                      |
| `amend`                  | All                       | Yes      | Stage all, amend last                     |
| `amend keep`             | All                       | Keep msg | Stage all, amend keep message             |
| `<submodule> amend`      | All in that submodule     | Yes      | Amend last commit in that submodule       |
| `staged amend`           | Staged only               | Yes      | Commit staged, amend last                 |
| `reword`                 | None                      | Msg only | Just update last message                  |

## Step 1: Detect Submodule Changes

**This step runs BEFORE staging. It determines the commit strategy.**

First, resolve the project's known submodules (see Step 1a lookup order). If the project declares no submodules and `.gitmodules` is absent, skip directly to Step 2.

```bash
# Check for dirty submodules (modified content or new commits)
git status
git submodule status 2>/dev/null
```

Classify changes into two buckets:

| Bucket              | What it contains                                                    |
| ------------------- | ------------------------------------------------------------------- |
| **Submodule changes** | Submodules with modified/untracked content or new commits           |
| **Parent changes**    | Files in the parent repo (non-submodule paths)                      |

**Decision tree:**

- **Only parent changes** → Skip to Step 2 (normal commit flow)
- **Submodule changes present** → Enter submodule commit flow (Step 1a)

### Step 1a: Commit Submodules One-by-One

For EACH dirty submodule (in dependency order if known, otherwise alphabetical):

1. `cd <submodule-dir>`
2. **Check for detached HEAD** — run `git rev-parse --abbrev-ref HEAD`:
   - If result is `HEAD` (detached), reattach before committing:
     ```bash
     git checkout main && git merge --ff-only HEAD@{1}
     ```
   - If reattach fails, warn the user and skip this submodule
3. Run the full commit flow (Steps 2–6) **inside the submodule**:
   - `git status` / `git diff` to analyze changes
   - Stage, craft message, commit — following all rules below
   - The commit type/scope should reflect what changed in THAT submodule
4. `cd` back to parent repo root

**IMPORTANT:**
- Each submodule gets its OWN independent commit with its own message
- Do NOT bundle multiple submodules into one commit
- If a submodule only has "new commits" (pointer moved) but no dirty content, skip committing inside it
- **Always ensure the submodule is on a branch (not detached HEAD) before committing** — detached commits cannot be pushed and will cause sync failures on other machines
- After all submodule commits, proceed to Step 1b

### Step 1b: Update Submodule Pointers in Parent

After all submodules are committed:

```bash
# Stage the updated submodule pointers
git add <submodule1> <submodule2> ...

# If there are also parent-repo file changes, stage those too (per staging intent)
# Then commit the parent repo — the message should reference the submodule updates
```

The parent commit message should summarize what was updated. Example (generic):

```
chore: sync submodule pointers

- <submodule-a>: feat(<scope>): <subject>
- <submodule-b>: fix(<scope>): <subject>
```

If there are ALSO parent-repo file changes mixed in, combine them into a single parent commit that includes both the pointer updates and the parent changes. The message should cover all changes.

## Step 2: Execute Staging

Based on parsed intent:

```bash
# Check current state
git status

# If "staged only" intent:
#   - Verify git diff --cached has content
#   - Do NOT run git add

# If selective staging (ANY non-empty, non-amend-only input):
#   1. Get changed files from git status
#   2. Match input keywords against file names/paths
#   3. If "related" keyword: include test files, tightly-coupled imports
#   4. git add <matched-files-only>

# If empty input (no arguments at all):
#   - git add -A (stage all)
```

**CRITICAL:** `git add -A` is ONLY for empty input. Any non-empty input means selective staging.

## Step 3: Analyze Changes

```bash
# See what will be committed
git diff --cached --stat
git diff --cached

# For amend mode, also check last commit
git log -1 --stat
git show HEAD
```

## Step 4: Determine Change Scope

| Scope       | Criteria                  | Message Format        |
| ----------- | ------------------------- | --------------------- |
| Single file | 1 file changed            | Subject only (1 line) |
| Small       | 2-5 files                 | Subject + 2-3 bullets |
| Big         | 6+ files or major feature | Subject + 3-5 bullets |

## Step 5: Select Commit Type

| Type       | When to Use                                |
| ---------- | ------------------------------------------ |
| `feat`     | New feature, new capability                |
| `fix`      | Bug fix, error correction                  |
| `refactor` | Code restructuring without behavior change |
| `docs`     | Documentation only                         |
| `style`    | Formatting, whitespace                     |
| `test`     | Adding/updating tests                      |
| `chore`    | Maintenance, dependencies, config, tooling |
| `perf`     | Performance improvements                   |
| `ci`       | CI/CD configuration                        |
| `build`    | Build system, dependencies                 |
| `bump`     | Version bump                               |

**Scope:** Add in parentheses for clarity: `feat(auth):`, `fix(api):`

## Step 6: Craft Commit Message

### Format

```
type(scope): concise subject line (≤50 chars ideal, ≤72 max)

- Bullet point explaining what changed
- Focus on WHAT and WHY, not HOW
```

### Rules

1. **Subject Line**
   - Imperative mood: "Add feature" not "Added feature"
   - Lowercase after type
   - No period at end
   - Be specific

2. **Body Bullets** (when needed)
   - Start with `-`
   - Each bullet is a complete thought
   - Include searchable keywords

## Step 7: Execute Commit

### New Commit

```bash
git commit -m "$(cat <<'EOF'
type(scope): subject line

- First bullet point
- Second bullet point
EOF
)"
```

### Amend (update message)

```bash
git commit --amend -m "$(cat <<'EOF'
type(scope): updated subject line

- Combined change description
EOF
)"
```

### Amend Keep Message

```bash
git commit --amend --no-edit
```

### Reword Only

```bash
# No staging, just update message
git commit --amend -m "$(cat <<'EOF'
type(scope): improved subject line

- Better description
EOF
)"
```

## Step 8: Verify

```bash
git log -1
git status

# If submodules were committed, also verify:
git submodule status
```

## Pre-flight Checks

1. **No changes:** If nothing to commit, inform user and stop
2. **Staged only intent:** Verify `git diff --cached` has content
3. **Amend intent:** Verify there IS a previous commit
4. **Reword intent:** Proceed even with no staged changes
5. **Submodule changes:** Detect and commit submodules before parent

## Quick Reference

```bash
/dev-commit                          # Stage all, submodules first, then parent
/dev-commit staged                   # Only staged files
/dev-commit <submodule>              # Commit that submodule only, update parent ptr
/dev-commit <submodule> parser fixes # Commit submodule with message hint
/dev-commit <sub-a> and <sub-b>      # Commit both submodules, then parent ptrs
/dev-commit <submodule> amend        # Amend last commit in that submodule
/dev-commit workflow related         # workflow.py + test file
/dev-commit @src/lib/auth.py fixes   # auth.py + related files
/dev-commit prompts                  # prompts/** changes only
/dev-commit staged and tools         # Staged + tools/**
/dev-commit amend                    # Add all to last commit
/dev-commit amend keep               # Add all to last, keep message
/dev-commit staged amend             # Add staged to last commit
/dev-commit reword                   # Fix last commit message only
```

## Anti-Patterns

- "Update file" - Too vague
- "Fix bug" - Which bug? Where?
- "WIP" - Not for permanent commits
- Mixing unrelated changes in one commit
- Committing parent pointer update without committing submodule first
