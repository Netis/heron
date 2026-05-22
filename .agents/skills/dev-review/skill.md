---
version: 1.0.1
description: "Run code review for all configured languages. Usage: /dev-review [language]"
---

# Code Review Orchestrator

Run comprehensive code review across all configured languages, or review a specific language.

## Parameters

- **Language** (optional): {{language}}
  - If provided: Review only that language (e.g., `js`, `py`, `an`)
  - If empty: Review all languages configured in project.yaml

## Step 0: Load Project Configuration

Read `project.yaml` to get review configuration:

```yaml
# Expected structure in project.yaml
code_review:
  pre_check: just quality all
  post_check: just test all
  languages:
    - id: py
      name: Python
      skill: dev-review:py
      paths: [src/]
      quality_cmd: just quality py
    - id: ts
      name: TypeScript
      skill: dev-review:ts
      paths: [src/]
      quality_cmd: just quality ts
    - id: rs
      name: Rust
      skill: dev-review:rs
      paths: [src/]
      quality_cmd: just quality rs
```

**Note**: The language list is project-specific. Read `project.yaml`'s `code_review.languages` at runtime to determine which languages to review. Do not assume any fixed set of languages.

## Step 1: Determine Scope

### If `{{language}}` is provided:
- Find matching language config by `id`
- Run only that language's review

### If `{{language}}` is empty:
- Get all configured languages from project.yaml
- Run reviews sequentially for each

**Language ID mapping**: Read from `project.yaml`'s `code_review.languages[]` entries. Each entry has `id`, `name`, `skill`, `paths`, and `quality_cmd`. This project ships `rs` (Rust) and `ts` (TypeScript) review skills. Only review languages configured for this project.

## Step 2: Pre-flight Check

Before running any review:

1. Check for uncommitted changes:
   ```bash
   git status --short
   ```

2. Run overall quality check:
   ```bash
   just quality all
   ```

3. If quality check fails, fix issues first before proceeding.

## Step 3: Run Language-Specific Reviews

For each language to review, invoke the corresponding skill:

For each language configured in `project.yaml`, invoke the corresponding skill:

```
Invoke skill: dev-review:<id> (or read .Codex/skills/dev-review/<id>.md)
```

The language-specific skill file contains the focus areas, best practices, and review checklist for that language. Refer to it for details rather than duplicating guidance here.

## Step 4: Aggregate Results

After each review completes, collect:

1. **Files Modified**: List from each review
2. **Issues Found**: By category
3. **Issues Fixed**: Automatically resolved
4. **Manual Attention**: Items needing human review

### Aggregation Template

```
Code Review Summary
===================

Languages Reviewed: {list from project.yaml}

{Language Name}:
  Files: N modified
  Fixed: {summary of fixes}
  Manual: {items needing human review, or "None"}

(repeat for each language)

Overall:
  Total files: N
  Total fixes: N
  Tests passing: Yes/No
```

## Step 5: Final Quality Check

After all reviews complete:

```bash
# Run full quality check
just quality all

# Run post-check from project.yaml (e.g., just test all)
$POST_CHECK_CMD
```

## Step 6: Report & Commit

If changes were made and all tests pass:

1. Generate summary report (as shown in Step 4)
2. Invoke `/dev-commit` with appropriate message:

```
refactor: code review improvements

- {Language}: {summary of changes}
- {Language}: {summary of changes}
```

## Usage Examples

```bash
# Review all configured languages
/dev-review

# Review only a specific language (by id from project.yaml)
/dev-review py
/dev-review ts
/dev-review rs
```

## Common Patterns Across Languages

These checks apply to ALL languages:

| Category | Check |
|----------|-------|
| Constants | No magic numbers/strings |
| Security | No hardcoded secrets |
| Duplicates | No copy-pasted code blocks |
| Structure | Proper organization |
| Tests | Independence, proper setup/teardown |
| Docs | Meaningful names and comments |

## Project Configuration Template

Add this to `project.yaml` for your project:

```yaml
code_review:
  # Run quality check before reviews
  pre_check: just quality all

  # Post-check after all reviews
  post_check: just test p0

  # Language-specific review configuration
  languages:
    - id: py
      name: Python
      skill: dev-review:py
      paths: [src/, scripts/]
      quality_cmd: just quality py
      enabled: true

    - id: ts
      name: TypeScript
      skill: dev-review:ts
      paths: [src/]
      quality_cmd: just quality ts
      enabled: true

    - id: rs
      name: Rust
      skill: dev-review:rs
      paths: [src/]
      quality_cmd: just quality rs
      enabled: true
```

## Checklist

```
[ ] Read project.yaml for review configuration
[ ] Determine scope (all languages or specific)
[ ] Pre-flight: git status clean, quality check passes
[ ] Run each language-specific review
[ ] Collect and aggregate results
[ ] Final quality check
[ ] All tests passing
[ ] Commit with descriptive message
```
