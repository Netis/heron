---
version: 0.9.1
description: "Verify commits merged and remove worktree safely"
---

# Remove Action

Verify all commits have been integrated, then safely remove the worktree and branch.

## Where to Run

**CRITICAL**: Remove must run from **root repo only**, not from within a worktree.

**Why**: Remove deletes the worktree directory. If you run it from within that worktree, your session's working directory becomes invalid, causing errors like `getcwd: cannot access`.

## Input Examples

```
/dev-worktree remove
/dev-worktree remove normalize-batch1
/dev-worktree finish
/dev-worktree cleanup fix-spd
```

## Tool Command

Use `just wt remove` for removal:

```bash
# Execute removal (removes worktree)
just wt remove <name>

# Manual pre-removal checks
git -C ".worktrees/<name>" status --porcelain         # Uncommitted changes?
base=$(cat ".worktrees/<name>/.wt_base")
git -C ".worktrees/<name>" log --oneline "$base..HEAD" # Unmerged commits?
```

## Workflow

### Step 0: Verify Running from Root Repo

```bash
main_repo=$(git worktree list | head -1 | awk '{print $1}')
if [ "$(pwd)" != "$main_repo" ]; then
    echo "In worktree — must run from root repo"
fi
```

If in a worktree, show instructions to return to root repo:

```
⚠ Cannot run remove from within a worktree.

Why: Remove deletes this directory, which invalidates your session.

Steps to remove '<name>':
  1. Exit this Claude session (Ctrl+C or 'exit')
  2. In terminal: cd <main_repo>
  3. Start new session: claude
  4. Run: /dev-worktree remove <name>
```

### Step 1: Determine Which Worktree

```bash
just wt list
```

If user specified a name, use it. Otherwise:
- If only one worktree exists (besides main), use it
- If multiple exist, ask which one

### Step 2: Preview Removal (Pre-checks)

**Always check before removing:**

```bash
# Uncommitted changes?
git -C ".worktrees/<name>" status --porcelain

# Unmerged commits?
base=$(cat ".worktrees/<name>/.wt_base" 2>/dev/null || git -C ".worktrees/<name>" merge-base HEAD "$(git rev-parse --abbrev-ref HEAD)")
git -C ".worktrees/<name>" log --oneline "$base..HEAD"
```

### Step 3: Handle Issues

If pre-checks reveal problems:

**Uncommitted changes:**
```
The worktree has uncommitted changes:
  M file1.js
  ? file2.js

Options:
1. Commit changes first
2. Discard changes: git -C ".worktrees/<name>" checkout .
3. Cancel cleanup
```

**Unmerged commits:**
```
⚠ UNMERGED COMMITS DETECTED

The following commits have not been integrated:
  ✗ xyz7890 test: fix edge case
  ✗ abc1234 test: add missing step

Options:
1. Merge first (/dev-worktree merge)
2. Force removal - DESTRUCTIVE
3. Cancel removal
```

**Important:** Never force-remove without explicit user consent.

### Step 4: Execute Removal

```bash
just wt remove <name>
# Then clean up the branch and prune
git branch -D "feature/<name>"
git worktree prune
```

### Step 5: Report Result

```bash
just wt list
```

Output:
```
✓ Worktree removed: <name>
  Branch: feature/<name>
  Steps:
    ✓ check_uncommitted: No uncommitted changes
    ✓ verify_merged: All commits merged
    ✓ remove_worktree: Removed: <path>
    ✓ delete_branch: Deleted branch: feature/<name>
    ✓ prune: Pruned orphaned worktree references

Remaining worktrees:
  (main)   6.0/develop   ✓ clean
```

## Quick Reference

| Task | Command |
|------|---------|
| List worktrees | `just wt list` |
| Check uncommitted | `git -C .worktrees/<name> status --porcelain` |
| Execute removal | `just wt remove <name>` |
| Delete branch | `git branch -D feature/<name>` |

## Error Handling

| Error | Action |
|-------|--------|
| `not_found` | List available, suggest correct name |
| `uncommitted_changes` | Ask: commit/discard/cancel |
| `unmerged_commits` | Warn, offer merge or force (requires consent) |
| `remove_failed` | Show git error, suggest manual removal |
| `getcwd: cannot access` | Session in removed worktree - restart |

## Safety Guarantees

1. **Dry-run first** - Always preview before executing
2. **Verify merged** - Tool verifies all commits before removal
3. **Force requires consent** - Never use --force without explicit user approval
4. **Atomic steps** - Each step reports success/failure
5. **Uses `-D` for branches** - Handles cherry-picked commits correctly
