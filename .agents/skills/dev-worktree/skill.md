---
version: 0.6.4
description: "Parallel development with git worktrees. Usage: /dev-worktree [action] [natural language]"
---

# Dev-Worktree Skill

Manage git worktrees for parallel development with multiple Codex sessions.

## Session Boundaries

**CRITICAL**: This skill manages worktrees but does NOT spawn agents to work inside them.

| Action | Where to Run | Spawns Agents? |
|--------|--------------|----------------|
| add | Root repo | No - outputs terminal command |
| list | Root or Worktree | No - read only |
| plan | Root repo | No - read only |
| merge | Root OR Worktree | No - git ops only |
| remove | Root repo only | No - git ops only |

**Why Session Isolation Matters**:

Codex has documented issues when operating across worktree boundaries:
- [#14652](https://github.com/anthropics/Codex/issues/14652): Lock acquisition failures in multi-process scenarios
- [#12932](https://github.com/anthropics/Codex/issues/12932): Worktree sync problems
- [#10105](https://github.com/anthropics/Codex/issues/10105): Directory confusion

See Research #0006 for full analysis.

**Correct Workflow**:
```
Root Codex:  /dev-worktree add foo
              → prints: cd .worktrees/foo && Codex

New Terminal: cd .worktrees/foo && Codex
              → work, commit

Either:       /dev-worktree merge
              → cherry-pick to main

Root Codex:  /dev-worktree remove foo
              → remove worktree
```

## Forbidden Actions in Worktree

**NEVER run these commands in a worktree:**

| Command | Why Forbidden |
|---------|---------------|
| `just quality all` | Reformats untouched files → merge conflicts |
| `/dev-lint-fix` | Same - reformats beyond your changes |
| `npm run lint:fix` | Same |
| `npm run format` | Same |

**Why**: Lint/format tools reformat ALL files, not just modified ones. This creates spurious diffs in files you never touched, causing unnecessary merge conflicts.

**Correct Workflow**:
```
Worktree:     Focus on test implementation only
              → commit your changes

Main repo:    After merge, run quality checks
              → just quality all
              → /dev-lint-fix (if needed)
              → commit formatting fixes separately
```

## Tool

**IMPORTANT**: Use `just wt` for ALL worktree operations:

```bash
# Lifecycle
just wt add <name>       # Create worktree + branch (records base commit)
just wt merge <name>     # Cherry-pick commits to main repo
just wt remove <name>    # Remove worktree after merge

# Status
just wt list             # List all worktrees
just wt dev <name>       # Open Codex in worktree

# Git fallbacks for advanced queries
git worktree list                                          # Detailed worktree info
git -C ".worktrees/<name>" log --oneline <base>..HEAD      # Commits to merge
git -C ".worktrees/<name>" status --porcelain              # Uncommitted changes
```

## Step 0: Detect Context

**CRITICAL**: Always check context before actions:

```bash
# Check if current directory is a worktree
git rev-parse --show-toplevel   # Shows repo root
git worktree list               # Shows all worktrees with paths

# Determine if inside a worktree (compare pwd to main repo)
main_repo=$(git worktree list | head -1 | awk '{print $1}')
if [ "$(pwd)" != "$main_repo" ]; then
    echo "In worktree: $(basename "$(pwd)")"
else
    echo "In main repo"
fi
```

**Why**: Different actions have different requirements:
- `cleanup` must run from root repo (deletes cwd otherwise)
- `merge` works from either location

## Help Output (Default)

```
/dev-worktree <action> [args]

Actions:
  add <name>       Create worktree + branch
  list             Show worktree(s) state
  plan <task>      Analyze task, suggest split (→ brainstorming)
  merge            Check conflicts + integrate commits
  remove           Verify merged + remove worktree

Aliases:
  setup = add, status = list, cleanup = remove

Examples:
  /dev-worktree add normalize-batch1
  /dev-worktree I want to work on fixing auth tests
  /dev-worktree am I ready to merge?
```

## Intent Detection

### Step 1: Parse Input

If input is empty → Show help output above and stop.

### Step 2: Check for Explicit Action

| Keywords | Action | Route To |
|----------|--------|----------|
| add, create, new, start, setup, begin | add | add.md |
| list, status, show, ls, where, what | list | list.md |
| plan, split, divide, organize, "how to" | plan | plan.md |
| merge, cherry, integrate, done, ready | merge | merge.md |
| remove, delete, rm, clean, cleanup, finish | remove | remove.md |

If first word matches keyword → Route to action file with remaining input.

### Step 3: Context-Aware Inference (Natural Language)

If no explicit action keyword found:

```bash
# Detect context
main_repo=$(git worktree list | head -1 | awk '{print $1}')
in_worktree=$( [ "$(pwd)" != "$main_repo" ] && echo true || echo false )

# Count existing worktrees (excluding main)
just wt list
```

**Inference rules:**

| Context | Input Pattern | Inferred Action |
|---------|---------------|-----------------|
| No worktrees | task description | add |
| In worktree | "done", "ready", "merge" | merge |
| In worktree | "clean", "finish", "remove" | remove |
| Any | "status", "where", "list" | list |
| Any | "plan", "split", "how to divide" | plan |
| Has worktrees | ambiguous | ask user |

### Step 4: Ask If Ambiguous

If intent cannot be determined:

```
I detected multiple possible intents from: "<input>"

1. Setup a new worktree for this task
2. Check status of existing worktrees
3. [other relevant options based on context]

Which did you mean?
```

## Action Routing

Once action is determined, read and follow the corresponding action file:

- **add** → Read add.md, follow its workflow
- **list** → Read list.md, follow its workflow
- **plan** → Read plan.md, follow its workflow (invokes brainstorming)
- **merge** → Read merge.md, follow its workflow
- **remove** → Read remove.md, follow its workflow

Pass the remaining input (after action keyword) to the action workflow.

## Quick Reference

| I want to... | Command |
|--------------|---------|
| Start parallel work | `/dev-worktree add <name>` |
| See what's active | `/dev-worktree list` |
| Plan task split | `/dev-worktree plan <task>` |
| Finish and integrate | `/dev-worktree merge` |
| Remove after merge | `/dev-worktree remove` |
