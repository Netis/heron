---
version: 0.1.0
description: "Analyze task and suggest worktree split (invokes brainstorming)"
---

# Plan Action

Analyze a task and suggest how to split it across multiple worktrees for parallel development.

## Input Examples

```
/dev-worktree plan normalize 12 test files
/dev-worktree how should I split this refactoring task
/dev-worktree plan fix all auth and config modules
```

## Workflow

### Step 1: Parse Task Description

Extract task description from input:
- `plan normalize 12 test files` → task = "normalize 12 test files"
- `how should I split this refactoring` → task = "refactoring task"

If task unclear, ask:
```
What task do you want to plan for parallel work?

Example: "normalize preSteps for 10 test files"
```

### Step 2: Invoke Brainstorming

**Important:** This action invokes the `superpowers:brainstorming` skill.

Prompt for brainstorming:
```
Help plan parallel worktree work for: <task>

Context:
- Using git worktrees for isolation
- Each worktree = separate Claude Code session
- Goal: minimize conflicts, maximize parallelism

Please analyze:
1. What files are involved?
2. Are there dependencies between files?
3. What's the conflict risk?
4. How many worktrees make sense?

Output a suggested split with worktree names.
```

### Step 3: Analyze Files

Brainstorming will explore:

```bash
# Find relevant files based on task
# Example for "refactor service modules":
find src/services -name "*.ts" -exec grep -l "deprecated\|legacy" {} \;

# Group by module
ls -d src/*/

# Count functions per file
for f in src/**/*.ts; do
  count=$(grep -c "export function\|export const" "$f" 2>/dev/null || echo 0)
  echo "$f: $count exports"
done
```

### Step 4: Assess Conflict Risk

| Factor | Low Risk | High Risk |
|--------|----------|-----------|
| Same file | Different files | Same file in multiple worktrees |
| Shared imports | Independent modules | Shared Page Objects being modified |
| Config files | Not touched | .gitignore, CLAUDE.md modified |
| Cross-references | None | Test IDs, fixtures shared |

### Step 5: Output Suggested Split

```
Task Analysis: "refactor 12 service modules"

Files involved:
  auth/:    login.ts (154), session.ts (49), token.ts (45)
  users/:   profile.ts (11), settings.ts (8)
  data/:    parser.ts (32), transform.ts (28), validate.ts (25)
  config/:  loader.ts (22), schema.ts (18), env.ts (15), defaults.ts (12)

Suggested Split:
┌────────────────┬─────────────────────────────────┬──────────┐
│ Worktree       │ Files                           │ Est Size │
├────────────────┼─────────────────────────────────┼──────────┤
│ refactor-auth  │ login, session, token           │ ~248 lines│
│ refactor-users │ profile, settings               │ ~19 lines │
│ refactor-data  │ parser, transform, validate     │ ~85 lines │
│ refactor-config│ loader, schema, env, defaults   │ ~67 lines │
└────────────────┴─────────────────────────────────┴──────────┘

Conflict Risk: LOW
- No shared files between worktrees
- Independent modules
- No config file modifications

To start:
  /dev-worktree setup normalize-apc
  /dev-worktree setup normalize-uac
  /dev-worktree setup normalize-spd
  /dev-worktree setup normalize-sm

Then open separate terminal for each and run: claude
```

### Step 6: User Adjustment

After showing suggestion:
```
Adjust this split?
- Type new grouping to change
- Press enter to accept
- Type "fewer" for fewer worktrees
- Type "more" for more granular split
```

## Guidelines for Good Splits

| Guideline | Rationale |
|-----------|-----------|
| Group by module/directory | Natural boundaries |
| 50-150 tests per worktree | Manageable scope |
| No file overlap | Zero conflict risk |
| Balance workload | Parallel efficiency |
| Keep related files together | Context preserved |

## Output Requirements

Always include:
1. Files involved with counts
2. Suggested split table
3. Conflict risk assessment
4. Ready-to-use setup commands
