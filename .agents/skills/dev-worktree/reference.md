---
version: 0.3.1
description: "Git worktree best practices for parallel AI development"
---

# Worktree Reference

General best practices for using git worktrees with parallel Claude Code sessions.

## Anti-Patterns (What NOT to Do)

| Pattern | Problem | Solution |
|---------|---------|----------|
| Spawn agents from root to edit worktree files | Session hangs, agent drift, lock conflicts | Start NEW Claude session in worktree |
| Work in worktree from root session | Context confusion, CPU spikes | One session per project directory |
| Run cleanup from within worktree | Deletes own cwd, session breaks | Exit session, run cleanup from root |
| Switch directories mid-session | Context confusion | Stay in one project per session |
| Keep root session while spawning to worktree | Lock acquisition failures | Close root or use for monitoring only |

**Research**: See docs/research/0006-2026-01-31-worktree-claude-code-best-practices.md for evidence.

## Key Rules

### Directory Awareness

| Rule | Rationale |
|------|-----------|
| Always verify `pwd` before operations | Agents may drift to wrong directory |
| Use absolute paths when spawning agents | Prevents accidental main repo modifications |
| Check `git worktree list` to confirm location | Visual verification of context |

### File Isolation

**Never modify in worktree:**
- `.gitignore` - Shared configuration
- `CLAUDE.md` - Shared AI configuration
- `package.json` - Shared dependencies
- `project.yaml` - Shared project config
- `.claude/` folder - Skills, commands, agents

**If config change needed:**
1. Do it in main repo first
2. Pull/rebase in worktree if needed
3. Or wait until worktree is merged

### Commit Strategy

| Practice | Benefit |
|----------|---------|
| Small, atomic commits | Easier cherry-pick |
| Descriptive messages | Clear in main history |
| Squash before merge if >3 commits | Clean history |
| One logical change per commit | Reviewable |

## Conflict Categories

| Category | Risk Level | Example | Mitigation |
|----------|------------|---------|------------|
| Same file | **High** | Two worktrees edit parser.ts | Never assign same file |
| Shared config | **Medium** | Both touch .gitignore | Exclude from worktree scope |
| Import chain | **Low** | Both use Pages.xxx | Coordinate major changes |
| Cross-reference | **Low** | Test IDs, fixtures | Merge resolves automatically |

## Common Issues

### Agent Modifies Wrong Directory

**Symptom:** `git status` in main repo shows unexpected changes

**Cause:** Agent spawned without explicit working directory

**Fix:**
```bash
# In main repo - restore unintended changes
git restore <accidentally-modified-files>

# Copy to worktree if needed
cp <file> .worktrees/<name>/<file>

# In worktree - commit
cd .worktrees/<name>
git add <file>
git commit -m "description"
```

**Prevention:** Always verify agent's pwd, use absolute paths

### Cherry-Pick Conflict

**Symptom:** Conflict markers after `git cherry-pick`

**Fix:**
```bash
# See conflicting files
git status

# Edit files to resolve (remove conflict markers)
# <<<<<<< HEAD
# main branch version
# =======
# worktree version
# >>>>>>> commit-hash

# Mark resolved
git add <resolved-files>

# Continue
git cherry-pick --continue

# Or abort if needed
git cherry-pick --abort
```

### Worktree Locked

**Symptom:** `fatal: 'xxx' is already checked out`

**Fix:**
```bash
git worktree unlock .worktrees/xxx
```

### Branch Already Exists

**Symptom:** Cannot create worktree - branch exists

**Fix:**
```bash
# Check which worktree uses it
git worktree list

# Either use existing worktree or choose new branch name
git worktree add .worktrees/new-name -b feature/new-name
```

### Orphaned Worktree Reference

**Symptom:** Worktree listed but directory doesn't exist

**Fix:**
```bash
git worktree prune
```

## Workflow Quick Reference

```
1. SETUP (Root Claude)
   /dev-worktree setup <name>
   → prints: cd .worktrees/<name> && claude

2. WORK (New Terminal)
   cd .worktrees/<name> && claude
   → make changes, commit with /dev-commit

3. STATUS (Either Session)
   /dev-worktree status

4. MERGE (Either Session - no folder change needed)
   /dev-worktree merge
   → cherry-picks to main branch

5. EXIT WORK SESSION
   exit or Ctrl+C
   → back to shell, cd to root

6. CLEANUP (Root Claude Only)
   /dev-worktree cleanup <name>
   → removes worktree + branch
```

## Git Commands Reference

| Task | Command |
|------|---------|
| List worktrees | `git worktree list` |
| Create worktree | `git worktree add <path> -b <branch>` |
| Remove worktree | `git worktree remove <path>` |
| Prune orphans | `git worktree prune` |
| Unlock worktree | `git worktree unlock <path>` |

## Worktree Tool Commands

| Task | Command |
|------|---------|
| List worktrees | `just wt list` |
| Create worktree | `just wt add <name>` |
| Merge worktree | `just wt merge <name>` |
| Remove worktree | `just wt remove <name>` |
| Open claude in wt | `just wt dev <name>` |
| Commits to merge | `base=$(cat .worktrees/<name>/.wt_base); git -C .worktrees/<name> log --oneline $base..HEAD` |
| Check uncommitted | `git -C .worktrees/<name> status --porcelain` |

## When NOT to Use Worktrees

| Scenario | Better Alternative |
|----------|-------------------|
| Single file change | Direct edit in main repo |
| 2-3 independent files | Parallel agents in single session |
| Quick bug fix | Direct commit in main repo |
| Exploratory work | Branch in main repo |

**Use worktrees for:**
- Large tasks (10+ files)
- Parallel team/AI work
- Risky experiments
- Long-running features
