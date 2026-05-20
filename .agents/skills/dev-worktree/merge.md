---
version: 0.10.1
description: "Check conflicts and integrate commits via cherry-pick into the main repo's active branch"
---

# Merge Action

Check for conflicts and integrate worktree commits into the main repo's **currently-checked-out branch** via cherry-pick.

**Destination branch:** whatever branch the main working tree has checked out when you run the merge.
- Main repo on `main` → commits land on `main`
- Main repo on `feature/capture` → commits land on `feature/capture`

This enables stacked workflows (worktree off a feature branch, merge back into that feature branch) without changing commands.

## Where to Run

Merge can run from **either** location - no need to change directories:

| Context | Behavior |
|---------|----------|
| **Root repo** | Specify worktree name or select from list |
| **Inside worktree** | Auto-detect current worktree, merge to main repo's active branch |

## Input Examples

```
/dev-worktree merge
/dev-worktree ready to integrate
/dev-worktree done with this worktree
/dev-worktree merge normalize-batch1
```

## Tool Command

Use `just wt merge` for merge operations:

```bash
# Execute merge (cherry-picks all commits to main repo)
just wt merge <name>

# Preview what will be cherry-picked (manual)
base=$(cat ".worktrees/<name>/.wt_base")
git -C ".worktrees/<name>" log --oneline "$base..HEAD"
```

## Workflow

### Step 1: Detect Context

```bash
main_repo=$(git worktree list | head -1 | awk '{print $1}')
in_worktree=$( [ "$(pwd)" != "$main_repo" ] && echo true || echo false )
```

**Context handling:**

| inWorktree | Name arg | Action |
|------------|----------|--------|
| true | - | Use current worktree |
| false | provided | Use specified worktree |
| false | - | List available, ask which |

If not in worktree and no name specified:
```bash
just wt list
```

### Step 2: Check for Uncommitted Changes

**Before previewing merge, check for uncommitted changes:**

```bash
git -C ".worktrees/<name>" status --porcelain
```

If there are uncommitted changes (output is not empty):

1. **Auto-commit using /dev-commit** - Invoke the `/dev-commit` skill to commit the changes
2. After commit completes, continue to Step 3

This ensures all work is captured before merge, without requiring manual user intervention.

### Step 3: Preview Merge (Dry Run)

**Always preview first:**

```bash
# Read the recorded base commit
base=$(cat ".worktrees/<name>/.wt_base")
git -C ".worktrees/<name>" log --oneline "$base..HEAD"
```

Count commits; if > 3, suggest squashing:
```
You have N commits. Consider organizing before merge:
  git -C ".worktrees/<name>" rebase -i HEAD~N

Options:
1. Squash commits first (recommended)
2. Continue with N separate commits
3. Cancel
```

### Step 3b: Check for Conflicts (Optional)

```bash
# Check which files the worktree changed vs base
base=$(cat ".worktrees/<name>/.wt_base")
git -C ".worktrees/<name>" diff --name-only "$base..HEAD"

# Compare with main repo changes since the same base
git diff --name-only "$base..HEAD"

# Overlapping files = potential conflicts
```

If conflicts detected, warn user before proceeding.

### Step 4: Execute Merge

```bash
just wt merge <name>
```

The tool:
1. Reads the recorded base commit from `.wt_base`
2. Gets commits in oldest-first order
3. Cherry-picks each commit to main repo
4. Reports success or conflict

**On conflict**, the tool returns:
- `error: cherry_pick_failed`
- `failedCommit`: the commit that conflicted
- `completedCommits`: commits already applied
- `message`: resolution instructions
- `abortCommand`: command to abort

### Step 4b: Handle Conflict (if cherry-pick fails)

When `cherry_pick_failed` occurs:

1. **Show conflicting files:**
   ```bash
   git -C <main_repo> diff --name-only --diff-filter=U
   ```

2. **Present options:**
   ```
   ⚠ Conflict on commit <hash>: <message>
   
   Conflicting files:
     ✗ path/to/file1.rs
     ✗ path/to/file2.go
   
   Options:
   1. Resolve manually — I'll guide you through each file
   2. Abort cherry-pick — undo and try a different approach
   3. Skip this commit — continue with remaining commits
   ```

3. **Option 1 — Manual resolution:**
   - For each conflicting file, read it and show the conflict markers
   - Help the user decide which version to keep
   - After all files resolved:
     ```bash
     git -C <main_repo> add <resolved_files>
     git -C <main_repo> cherry-pick --continue
     ```
   - Continue cherry-picking remaining commits

4. **Option 2 — Abort:**
   ```bash
   git -C <main_repo> cherry-pick --abort
   ```
   Suggest alternative: rebase worktree onto latest main first, then retry merge.

5. **Option 3 — Skip:**
   ```bash
   git -C <main_repo> cherry-pick --skip
   ```
   Continue with remaining commits. Warn that skipped commit's changes are lost.

### Step 5: Report Result

On success:
```
✓ Merge: <name>
  Cherry-picked N commit(s)
    ✓ abc1234 refactor: extract auth login helper
    ✓ def5678 refactor: simplify session handler

Next steps:
  cd <main_repo_path>           # Switch to main repo
  just quality all              # Run quality checks (forbidden in worktree)
  /dev-worktree remove <name>   # Remove worktree
```

**If running from worktree**: After merge completes, output the `cd` command to switch to main repo:
```bash
main_repo=$(git worktree list | head -1 | awk '{print $1}')
echo "cd $main_repo"
```

This is important because:
- Quality/lint commands are **forbidden in worktree** (cause merge conflicts)
- Cleanup must run from root repo

## Quick Reference

| Task | Command |
|------|---------|
| Detect context | `git worktree list` |
| List worktrees | `just wt list` |
| Preview merge | `cat .worktrees/<name>/.wt_base && git -C .worktrees/<name> log --oneline $(cat .worktrees/<name>/.wt_base)..HEAD` |
| Execute merge | `just wt merge <name>` |
| Check conflicts | Compare `git -C .worktrees/<name> diff --name-only <base>..HEAD` with `git diff --name-only <base>..HEAD` |

## Error Handling

| Error | Action |
|-------|--------|
| `not_found` | List worktrees, suggest correct name |
| `uncommitted_changes` | Ask: commit/stash/discard/cancel |
| `cherry_pick_failed` | Show resolution steps, offer abort command |
| No commits to merge | Inform, suggest cleanup |

## User Decision Points

1. **Uncommitted changes** → commit/stash/discard/cancel
2. **Many commits (>3)** → squash first or continue
3. **Potential conflicts** → proceed or cancel
4. **Actual conflict** → guide through resolution or abort
