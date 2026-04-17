---
version: 1.0.2
tier: agnostic
description: "Fix format, lint, and type issues in a loop. Usage: /dev-lint-fix [investigate=true]"
---

# Lint Fix Loop

Run quality checks (format + lint + types) and fix all issues iteratively until clean.

## Quality Check Categories

| Category | Tools | Auto-fixable |
|----------|-------|--------------|
| **Format** | Prettier, Black, rustfmt | Yes (100%) |
| **Lint** | ESLint, Ruff, Clippy | Partial (~70%) |
| **Types** | TypeScript, mypy, pyright | Manual (~10%) |

## Parameters

- `investigate`: {{investigate}} - When `true`, analyze root cause and fix/report

## Phase 0: Load Project Config

Read `project.yaml` to get quality commands:

```yaml
# Expected config
code_review:
  pre_check: just quality all  # Quality check command

  languages:
    - id: js
      quality_cmd: just quality js
    - id: py
      quality_cmd: just quality py
```

**Fallback if no project.yaml:**
```bash
# Try common quality commands
just quality all 2>/dev/null || \
npm run lint 2>/dev/null || \
npx eslint . 2>/dev/null || \
echo "No quality command found"
```

---

## Phase 1: RUN - Execute Quality Check

```bash
# Read from project.yaml or use default
QUALITY_CMD=$(yq -r '.code_review.pre_check // "npm run lint"' project.yaml 2>/dev/null)
$QUALITY_CMD 2>&1 | tee /tmp/lint-output.txt
```

**Capture output for analysis:**
- Exit code (0 = clean, non-zero = issues)
- Error messages and file paths
- Warning count and error count

---

## Phase 2: FIX - Resolve Issues

### 2.1 Fix Order: Format → Lint → Types

**Always fix in this order** (formatting changes can affect lint, lint can affect types):

```
┌──────────┐    ┌──────────┐    ┌──────────┐
│  FORMAT  │───▶│   LINT   │───▶│  TYPES   │
└──────────┘    └──────────┘    └──────────┘
   100%            ~70%           ~10%
  auto-fix       auto-fix       manual
```

### 2.2 Format Fixes (100% auto-fixable)

```bash
# JavaScript/TypeScript
npx prettier --write "**/*.{js,ts,tsx,json,md,yaml}" 2>/dev/null

# Python
ruff format . 2>/dev/null || black . 2>/dev/null

# Rust
cargo fmt 2>/dev/null

# Go
gofmt -w . 2>/dev/null
```

### 2.3 Lint Fixes (~70% auto-fixable)

```bash
# JavaScript/TypeScript
npx eslint . --fix 2>/dev/null

# Python
ruff check --fix . 2>/dev/null

# Rust
cargo clippy --fix --allow-dirty 2>/dev/null

# Go
golangci-lint run --fix 2>/dev/null
```

**Manual lint fixes:**

| Issue | Fix |
|-------|-----|
| `no-unused-vars` | Remove or use the variable |
| `no-explicit-any` | Add proper type annotation |
| `complexity` | Refactor function |
| `import/order` | Reorder imports per convention |

### 2.4 Type Fixes (~10% auto-fixable)

Type errors usually require manual intervention:

```bash
# TypeScript - check only, no auto-fix
npx tsc --noEmit 2>&1 | head -50

# Python
mypy . 2>/dev/null || pyright . 2>/dev/null
```

**Common type fixes:**

| Error | Fix |
|-------|-----|
| `Type 'X' is not assignable` | Add type assertion or fix type |
| `Property 'x' does not exist` | Add to interface or use optional chain |
| `Object is possibly undefined` | Add null check |
| `Argument type mismatch` | Cast or fix function signature |

### 2.5 Manual Fixes

For issues that can't be auto-fixed:

1. **Read the error message** - Extract file path and line number
2. **Read the file** - Understand context
3. **Apply minimal fix** - Don't refactor, just fix the issue
4. **Verify** - Re-run quality check on that file

**Common manual fixes:**

| Issue | Fix |
|-------|-----|
| Unused variable | Remove declaration or use it |
| Missing return type | Add explicit type annotation |
| Unreachable code | Remove or fix control flow |
| Console.log left in | Remove or add eslint-disable |

---

## Phase 3: LOOP - Repeat Until Clean

```
┌─────────────────────────────────────────┐
│           LINT FIX LOOP                 │
├─────────────────────────────────────────┤
│                                         │
│  ┌─────────┐                            │
│  │   RUN   │◀────────────┐              │
│  └────┬────┘             │              │
│       │                  │              │
│       ▼                  │              │
│  ┌─────────┐         ┌───┴───┐          │
│  │ Issues? │───Yes──▶│  FIX  │          │
│  └────┬────┘         └───────┘          │
│       │                                 │
│      No                                 │
│       │                                 │
│       ▼                                 │
│  ┌─────────┐                            │
│  │  DONE   │                            │
│  └─────────┘                            │
│                                         │
│  Max iterations: 5                      │
│  (prevent infinite loops)               │
└─────────────────────────────────────────┘
```

**Loop rules:**
- Max 5 iterations (prevent infinite loops)
- Track which issues were fixed vs persistent
- If same issue persists 2+ iterations, mark as "needs investigation"

---

## Phase 4: INVESTIGATE (when investigate=true)

When `{{investigate}}` is `true`, analyze WHY issues occurred.

### 4.1 Identify Root Cause

| Pattern | Root Cause | Prevention |
|---------|------------|------------|
| Same error type across files | Missing project rule | Add to config |
| New pattern not in guide | Knowledge gap | Update agent docs |
| Generated code has issues | Generator needs fix | Fix generator |
| Third-party integration | External tooling | Document exception |

### 4.2 Check Recent Changes

```bash
# What changed recently?
git diff --name-only HEAD~5

# Who/what introduced the issue?
git log --oneline -10 --all -- <problem_file>
```

### 4.3 Check Generators/Agents

If issues were introduced by AI-generated code:

1. **Identify the source** - Which agent/command generated the code?
2. **Read agent docs** - Check `.claude/agents/*.md` for guidance
3. **Update agent** - Add ESLint rules to prevent future issues
4. **Bump version** - Update agent version in frontmatter

### 4.4 Investigation Report

```markdown
## Lint Investigation Report

### Issues Found
- 6 errors, 4 warnings in src/utils/parser.ts

### Root Cause Analysis
| Issue | Count | Source | Prevention |
|-------|-------|--------|------------|
| no-explicit-any | 4 | Code generation | Added type annotations to agent docs |
| no-unused-vars | 2 | Manual edit | Removed unused helpers |

### Actions Taken
1. Fixed all issues
2. Updated relevant agent docs with lint compliance section

### Recommendations
- Run `/dev-lint-fix` after test implementation
- Consider adding pre-commit hook for lint
```

---

## Output Format

### Success (no issues)
```
Lint Fix: CLEAN
No issues found.
```

### Fixed
```
Lint Fix: FIXED

Iteration 1: 10 errors, 4 warnings
- Auto-fixed: 8 (formatting, imports)
- Manual fix: 6 (unsafe chain, unused vars)

Iteration 2: 0 errors, 0 warnings
CLEAN after 2 iterations.

Files modified:
- src/utils/parser.ts (6 fixes)
- src/lib/config.ts (4 fixes)
```

### With Investigation
```
Lint Fix: FIXED + INVESTIGATED

Root cause: code generation agent missing lint guidance
Prevention: Updated agent docs with lint rules section
```

### Persistent Issues
```
Lint Fix: PARTIAL

Fixed: 8/10 issues
Persistent (needs manual review):
- src/legacy.js:45 - Complex type error
- src/legacy.js:89 - Dynamic import pattern

These issues require architectural decisions.
```

---

## Quick Reference

```bash
# Basic: fix all lint issues
/dev-lint-fix

# With investigation: find root cause
/dev-lint-fix investigate=true

# After implementing tests
/dev-lint-fix

# After discovering repeated issues
/dev-lint-fix investigate=true
```

---

## Integration with Other Commands

| After | Run | Why |
|-------|-----|-----|
| `/dev-commit` | Before commit | Ensure clean code |
| Test implementation | After writing | Catch issues early |
| `/dev-review` | Before review | Pre-clean code |
| Agent generates code | After generation | Verify output |

---

## Project-Specific Config

This command reads `project.yaml` for:

```yaml
code_review:
  # Main quality command
  pre_check: just quality all

  # Language-specific (for targeted fixes)
  languages:
    - id: js
      quality_cmd: just quality js
      paths: [src/]
    - id: py
      quality_cmd: just quality py
      paths: [src/]
```

**If no project.yaml**, uses sensible defaults:
- `just quality all` or `npm run lint` or `npx eslint .`
- `npx prettier --check .`

**Workspace projects**: If `project.yaml` has `type: workspace`, run quality checks per-submodule when possible. Read `code_review.languages[].paths` to determine scope.
