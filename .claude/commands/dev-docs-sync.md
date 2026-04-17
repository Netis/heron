---
version: 4.1.2
tier: agnostic
description: "Sync documentation with project state using ICAV workflow. Usage: /dev-docs-sync [cleanup=true]"
---

# Documentation Sync and Update

Synchronize documentation with actual project state using the ICAV (Inventory-Compare-Adapt-Validate) workflow.

## ICAV Workflow

```
INVENTORY → COMPARE → ADAPT → VALIDATE → COMMIT
```

**Core principle:** Project state is truth. Docs follow reality. Every doc claim must trace to an artifact.

## Preferred Tools (Priority Order)

| Task | Primary Tool | Fallback |
|------|--------------|----------|
| Find files | `Glob` | `ls` |
| Search patterns | `Grep` | - |
| List directories | `ls` | - |
| Read files | `Read` | - |

## Parameters

| Parameter | Values | Default | Description |
|-----------|--------|---------|-------------|
| `cleanup` | `true`, `false` | `false` | Remove completed TODOs and obsolete content |

---

# Phase 1: INVENTORY

**Goal:** Collect project facts and list documentation files.

## 1.1 Load Project Context

```bash
cat VERSION
git log --oneline -20
```

Read `project.yaml` for the documentation language:

```yaml
language:
  docs: English  # or Chinese / 中文
```

Default if missing: `English`. Aliases: `Chinese` ≡ `中文`. All documentation content written or edited in Phase 3 MUST be in `language.docs`. File names remain English regardless.

## 1.2 Inventory Project Artifacts

```bash
# Documentation files
ls docs/

# Repository references
ls repos/

# Repo metadata
ls repos-meta/ 2>/dev/null

# Slash commands
ls .claude/commands/*.md

# Skills
ls -d .claude/skills/*/ 2>/dev/null

# Project files
ls *.md VERSION .gitignore
```

## 1.3 Inventory Documentation Files

| File | Purpose |
|------|---------|
| `CLAUDE.md` | AI dev guide — conventions, structure, rules |
| `README.md` | Project overview — purpose, structure |
| `docs/XXX-*.md` | Design documents — technical specs, architecture |
| `CHANGELOG.md` | Release history (managed by `/dev-bump`) |

**Excluded from sync:**
- `CHANGELOG.md` (managed by `/dev-bump`)
- `repos/` and `repos-meta/` (external references, protected)

---

# Phase 2: COMPARE

**Goal:** Find mismatches between docs and reality.

**CRITICAL:** Each comparison must run a concrete command against a specific doc section. Do NOT skip any row.

## Source-of-Truth Map

### CLAUDE.md

| Doc Section | Source of Truth | Command |
|---|---|---|
| File naming convention | filesystem | `ls docs/` — verify all follow `XXX-name.md` pattern |
| Protected directories | filesystem | `ls repos/ repos-meta/` — verify they exist |
| Git commit rules | git log | `git log --oneline -5` — verify conventions followed |

### README.md

| Doc Section | Source of Truth | Command |
|---|---|---|
| Project description | docs/ contents | Compare README overview with actual doc topics |
| Directory structure | filesystem | `ls -la` vs README tree listing |
| Version | VERSION file | `cat VERSION` vs README version mention |

### docs/ files

| Doc Section | Source of Truth | Command |
|---|---|---|
| File numbering | filesystem | `ls docs/` — verify sequential, no gaps |
| Cross-references | docs content | `grep -r 'docs/' docs/` — verify internal links valid |
| Referenced repos | repos/ | `ls repos/` vs repo mentions in docs |

## 2.1 Git Diff Focus (Optional Acceleration)

```bash
git diff --name-only $(git describe --tags --abbrev=0 2>/dev/null || echo HEAD~20)..HEAD
```

Use to **prioritize** which rows to check first, but **always check all rows**.

---

# Phase 3: ADAPT

**Goal:** Fix every mismatch found in Phase 2.

For each mismatch:
1. Read the doc section
2. Read the source of truth
3. Edit the doc to match reality
4. Preserve surrounding formatting and style

**Rules:**
- Never change project artifacts to match docs — docs follow reality
- Preserve existing doc structure (tables stay tables, trees stay trees)
- Don't add new sections — only update existing content
- If a doc file is missing an entire topic, note it in the report but don't create new sections
- Write documentation content in the language from `project.yaml` → `language.docs` (default: English). File names remain English regardless of docs language.

## Cleanup (if cleanup=true)

- Remove completed TODO items
- Remove obsolete content referencing deleted features
- No dates, no status badges, compact formatting

---

# Phase 4: VALIDATE

**Goal:** Verify sync succeeded.

## 4.1 Re-run Failed Comparisons

For every mismatch fixed in Phase 3, re-run the comparison command to verify the fix.

## 4.2 Check Internal Links

```bash
# Find all markdown links in docs/
grep -rohP '\[.*?\]\(([^)]+)\)' docs/*.md | grep -oP '\(([^)]+)\)' | tr -d '()'
```

Verify each relative link target exists.

## 4.3 Generate Report

```
Documentation Sync Report
==========================

Comparisons Run: [N]
Mismatches Found: [N]
Mismatches Fixed: [N]

Changes:
- [file]: [what changed]
- [file]: [what changed]

Verified: [all fixed / N remaining]
```

## 4.4 Commit

```bash
/dev-commit "sync docs with project state"
```

---

# Checklist

```
INVENTORY
[ ] VERSION, git log loaded
[ ] Documentation files listed
[ ] Repository references inventoried
[ ] Slash commands listed

COMPARE (Source-of-Truth Map — every row)
[ ] CLAUDE.md: naming convention, protected dirs, git rules
[ ] README.md: description, structure, version
[ ] docs/ files: numbering, cross-references, repo mentions

ADAPT
[ ] All mismatches fixed
[ ] Cleanup applied (if requested)

VALIDATE
[ ] Fixed comparisons re-verified
[ ] Internal links valid
[ ] Report generated
[ ] Changes committed
```
