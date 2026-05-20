---
version: 0.6.1
description: "Show worktree(s) state and commit status"
---

# List Action

Display current state of worktree(s) including commits, changes, and next actions.

## Prerequisites

The tool works from any location - it auto-detects context.

## Input Examples

```
/dev-worktree list
/dev-worktree where am I
/dev-worktree status
/dev-worktree list normalize-batch1
```

## Workflow

### Step 1: Detect Context

```bash
main_repo=$(git worktree list | head -1 | awk '{print $1}')
if [ "$(pwd)" != "$main_repo" ]; then
    echo "In worktree: $(basename "$(pwd)")"
else
    echo "In main repo"
fi
```

### Step 2: List All Worktrees

```bash
just wt list
```

### Step 3: Get Details for Specific Worktree

If user requested specific worktree or in a worktree:

```bash
# Commits to merge (using recorded base)
base=$(cat ".worktrees/$name/.wt_base" 2>/dev/null || git -C ".worktrees/$name" merge-base HEAD "$(git rev-parse --abbrev-ref HEAD)")
git -C ".worktrees/$name" log --oneline "$base..HEAD"

# Uncommitted changes
git -C ".worktrees/$name" status --porcelain

# Changed files
git -C ".worktrees/$name" diff --name-only "$base..HEAD"
```

### Step 4: Output (Single Worktree)

When in a worktree or specific name requested:

```
Worktree: normalize-batch1
Path: /path/to/.worktrees/normalize-batch1
Branch: feature/normalize-batch1
Base: 6.0/develop

Commits (3):
  abc1234 refactor: extract auth login helper
  def5678 refactor: simplify session handler
  ghi9012 refactor: consolidate user profile utils

Working tree: clean

Changed files:
  M src/auth/login.ts
  M src/auth/session.ts

Actions available:
  /dev-worktree merge    - integrate commits to main
  /dev-worktree remove   - remove after merging
```

### Step 5: Output (All Worktrees)

When in main repo (from `just wt list`):

```
Worktrees:
┌─────────────────┬─────────────────────┬─────────┬─────────┐
│ Name            │ Branch              │ Commits │ Status  │
├─────────────────┼─────────────────────┼─────────┼─────────┤
│ normalize-batch1│ feature/normalize-b │ 3       │ clean   │
│ fix-spd         │ feature/fix-spd     │ 1       │ 2 files │
└─────────────────┴─────────────────────┴─────────┴─────────┘

Total: 2 worktrees

Use: /dev-worktree list <name> for details
```

### Step 6: No Worktrees

If no worktrees exist (list returns only main):

```
No active worktrees.

To create one:
  /dev-worktree add <name>

Example:
  /dev-worktree add fix-spd-tests
```

## Quick Reference

| Task | Command |
|------|---------|
| Detect context | `git worktree list` |
| List all | `just wt list` |
| Get details | `git -C ".worktrees/$name" log --oneline $(cat .worktrees/$name/.wt_base)..HEAD` |

## Output Format

- Use tables for multiple worktrees (scannable)
- Use detailed view for single worktree (actionable)
- Always show available actions
- Show command hints for next steps
