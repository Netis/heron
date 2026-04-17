---
version: 1.2.0
tier: agnostic
description: "Create a research report. Adaptive: delegates to a project-local `dev-deep-research` skill if installed, otherwise runs a built-in single-direction workflow. Usage: /dev-research <topic> [question]"
---

# Research Report

Create a structured research report investigating a tool, pattern, or technology.

**Adaptive behavior:** if the target project has a `dev-deep-research` skill installed at `.claude/skills/dev-deep-research/`, this command delegates to that skill (which typically runs multi-direction parallel research). Otherwise it runs the built-in single-direction workflow below.

## Step 0: Load Project Configuration

Read `project.yaml` for research paths and output language:

```yaml
research:
  dir: docs/research              # Research reports directory
  readme: docs/research/README.md # Research index

language:
  docs: English                    # Research reports are written in this language
```

Defaults if missing: `dir: docs/research`, `readme: docs/research/README.md`, `language.docs: English`. Aliases: `Chinese` ≡ `中文`.

Research reports serve both AI and humans. Write the report body, summary, findings, and README index entry in `language.docs`. File names and code/config snippets remain as-is — never translate identifiers, commands, or file paths.

## Step 1: Adaptive Skill Detection

Check whether the project has a richer research skill installed:

```bash
test -d .claude/skills/dev-deep-research
```

- **If present** → invoke the `dev-deep-research` skill via the Skill tool, passing `$ARGUMENTS` as input. That skill owns the entire flow (direction analysis, parallel investigation, structured report). Do not continue this command after delegating.
- **If absent** → continue with the built-in single-direction workflow below.

## Step 2: Parse Topic

From `$ARGUMENTS`, extract:
1. **Topic** — what to investigate (tool, pattern, technology)
2. **Question** — optional specific question

If topic is unclear, use `AskUserQuestion` to clarify before investigating.

## Step 3: Investigate

Gather evidence for the topic. Choose the investigation approach based on topic type:

| Topic type | Investigation approach |
|---|---|
| MCP / plugin / tool | Check config, test tools, compare to built-in alternatives |
| Pattern / practice | Grep the codebase for usage, review git history |
| Technology choice | Compare alternatives head-to-head, test integration |

For each, collect: usage evidence, functionality check, comparison to alternatives, value assessment. Always record the commands you ran so findings are reproducible.

## Step 4: Generate Report

Create `{research.dir}/{NNNN}-{date}-{slug}.md` where:

- `{NNNN}` — next 4-digit sequence from `{research.readme}` (start at `0001`)
- `{date}` — today in `YYYY-MM-DD`
- `{slug}` — kebab-case topic summary

### Report Template

```markdown
# {Topic}

**Date**: {YYYY-MM-DD}
**Status**: Complete
**Finding**: {one-line conclusion}

## Summary
{what was investigated and why}

## Evidence
{git history, configs, tests — include the commands used}

```bash
# Commands run to gather evidence
git log --grep="topic"
grep -r "pattern" src/
```

## Analysis
{interpretation of the evidence}

## Comparison (if applicable)

| Aspect | Option A | Option B | Winner |
|--------|----------|----------|--------|

## Recommendation

**{Keep / Remove / Modify}**: {action with rationale}
```

## Step 5: Update Index

Prepend an entry to the top of `{research.readme}` (newest first, number-descending order):

```markdown
| {NNNN} | {date} | [{title}](./{filename}) | Complete |
```

## Step 6: Report Back

```
Research report created: {NNNN}-{date}-{slug}.md

Finding:        {one-line}
Recommendation: {action}
```

## Notes

- **One finding per report** — split complex topics into separate reports.
- **Evidence-based** — include commands and paths; never rely on assumptions.
- **When the single-direction workflow isn't enough** (multi-direction needs like market + technology + internal code + historical lessons), install a `dev-deep-research` skill in the project. This command will auto-delegate once it exists.
