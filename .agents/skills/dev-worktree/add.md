---
version: 0.5.1
description: "Create worktree + branch for isolated development"
---

# Add Action

Create a new git worktree with dedicated branch for isolated parallel development.

## Input Examples

```
/dev-worktree add normalize-batch1
/dev-worktree create a worktree for fixing auth tests
/dev-worktree I need to work on config module separately
```

## Workflow

### Step 1: Parse Name

**Explicit name:**
- Input: `add normalize-batch1` → name = `normalize-batch1`

**Natural language:**
- Input: `for fixing auth tests` → Extract or ask:
  ```
  What should I name this worktree?
  Suggestion: fix-auth

  [Enter name or press enter for suggestion]
  ```

**Name validation:**
- No spaces (use hyphens)
- Valid git branch characters
- Not already in use

### Step 2: Create Worktree

```bash
just wt add "$name"
```

This will:
- Create the worktree at `.worktrees/$name`
- Create branch `feature/$name`
- Record the base commit in `.worktrees/$name/.wt_base` (used by merge)
- Print the path and base info

**Error handling:**
- Worktree already exists → ask to use or rename
- Branch already exists → ask to use or rename

### Step 3: Run Project Setup (in worktree)

If the project has dependencies, instruct the user to run setup in the new terminal:

```bash
# In the new worktree terminal
just dev setup    # or: npm install, uv sync, etc.
```

### Step 4: Output Success Message

**IMPORTANT**: Do NOT switch to the worktree in this session. Always direct user to start a new terminal.

**Why**: Claude Code has documented issues with multi-process lock acquisition and directory confusion when operating across worktree boundaries (see Research #0006).

Analyze the original task description for test-related keywords: fix test, test fix, failing test, etc.

```
✓ Worktree created successfully

Worktree: $name
Path: $worktree_path
Branch: feature/$name
Base: $baseBranch

To start working, open a NEW terminal and run:

  cd $worktree_path && claude

[If test-related task detected:]
Tip: Use /test-pdca <scope> for structured test development

Workflow:
1. Work in the new session (commits stay in worktree branch)
2. Use /dev-worktree merge when done (works from either session)
3. Return here for /dev-worktree remove after merging

Tips:
- Only modify files in your task scope
- Avoid shared configs (.gitignore, CLAUDE.md, package.json)
```

**Do NOT offer to work in this session**. The new terminal approach prevents:
- Session hangs from lock conflicts
- Agent drift to wrong directories
- Context confusion between worktrees

## Quick Reference

| Task | Command |
|------|---------|
| Create worktree | `just wt add $name` |
| List worktrees | `just wt list` |

## Error Handling

| Error | Action |
|-------|--------|
| Invalid name | Show valid format, ask again |
| Worktree exists | Ask: use existing or new name |
| Branch exists | Ask: use existing branch or rename |
| npm install fails | Warn but continue |

## Output

On success, always show:
1. Worktree name and full path
2. Branch name and base branch
3. **Terminal command** to start new Claude session (primary action)
4. Workflow reminder (merge → cleanup)
5. Tips for isolation
