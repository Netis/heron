---
version: 2.0.2
tier: agnostic
description: "Design and plan a feature. Usage: /dev-plan <feature or task description>"
---

# Design & Plan

Orchestrate feature design and implementation planning by delegating to official skills while managing project-specific structure.

## Parameters

- **Description**: $ARGUMENTS (what you want to design and implement)

## Workflow

```
/dev-plan "add X"
    │
    ▼
Step 0: Load project.yaml (paths, naming, next NNNN)
    │
    ▼
Step 1: superpowers:brainstorming
         → Explore, clarify, propose approaches
         → Output: design doc → docs/design/{NNNN}-{date}-{slug}.md
    │
    ▼
Step 2: superpowers:writing-plans
         → Bite-sized tasks with TDD, exact paths, commands
         → Output: impl plan → docs/plan/{NNNN}-{date}-{slug}-impl.md
    │
    ▼
Step 3: Update README.md indexes for both directories
    │
    ▼
Step 4: Offer execution (subagent-driven or parallel session)
```

---

## Step 0: Load Project Configuration

Read `project.yaml` for paths and language:

```yaml
design:
  dir: docs/design
  readme: docs/design/README.md

plan:
  dir: docs/plan
  readme: docs/plan/README.md

language:
  ai: Any       # Plan/design output language (plans are consumed by AI executors)
```

Defaults if missing: `docs/design`, `docs/plan`, `language.ai: Any`.

### Output Language

Design docs and implementation plans are primarily consumed by AI executors (`superpowers:executing-plans`, `superpowers:subagent-driven-development`). They follow `language.ai`:

| `language.ai` | Behavior |
|---|---|
| `English` | Write plan/design body in English |
| `Chinese` / `中文` | Write plan/design body in Chinese |
| `Any` / `任意` (default) | Choose whatever language matches the user's conversation and project context |

File names, code blocks, commands, and file paths remain as-is regardless of language setting — never translate identifiers.

### Determine Next Number

Read both README.md files, find the highest `NNNN` across both, then:
- Design gets `NNNN+1`
- Plan gets the same `NNNN+1`

Both docs share the same number for traceability (e.g., design `0003-...` links to plan `0003-...-impl`).

### File Naming

| Type | Pattern | Example |
|------|---------|---------|
| Design | `{NNNN}-{date}-{slug}.md` | `0003-2026-02-18-user-auth.md` |
| Plan | `{NNNN}-{date}-{slug}-impl.md` | `0003-2026-02-18-user-auth-impl.md` |

---

## Step 1: Brainstorming → Design Document

**Invoke `superpowers:brainstorming`** with these project-specific overrides:

### Override: Save Location

The brainstorming skill defaults to `docs/plans/YYYY-MM-DD-<topic>-design.md`.
**Override**: Save to `{design.dir}/{NNNN}-{date}-{slug}.md` instead.

### Override: Design Document Template

Use this template (matches our existing design docs):

```markdown
# {Title}

**Created**: {YYYY-MM-DD}
**Status**: Draft
**Related**:
- [Implementation Plan]({plan.dir}/{NNNN}-{date}-{slug}-impl.md)

## Overview

{Brief description}

## Problem Statement

{What problem? Why needed?}

## Requirements

### Must Have
- [ ] Requirement 1

### Nice to Have
- [ ] Optional 1

## Proposed Approach

{High-level solution}

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| {point} | {choice} | {why} |

### Architecture

{Components, data flow, diagrams}

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| {risk} | H/M/L | {mitigation} |

## Open Questions

- [ ] Question?
```

### When UI Work is Involved

If the feature involves frontend/UI, also invoke `frontend-design` during brainstorming for:
- Typography, color, and layout direction
- Component structure and interaction design
- The design doc should capture these visual decisions

### Brainstorming Checklist

The skill handles its own process. Verify these are covered:

```
[ ] Project context explored
[ ] Clarifying questions asked (one at a time)
[ ] 2-3 approaches proposed with trade-offs
[ ] Design presented and approved by user
[ ] Design doc written to {design.dir}/
```

---

## Step 2: Writing Plans → Implementation Plan

**Invoke `superpowers:writing-plans`** with these project-specific overrides:

### Override: Save Location

The skill defaults to `docs/plans/YYYY-MM-DD-<feature-name>.md`.
**Override**: Save to `{plan.dir}/{NNNN}-{date}-{slug}-impl.md` instead.

### Override: Plan Header

Add a link back to the design doc:

```markdown
# {Title} Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** {one sentence}

**Architecture:** {2-3 sentences}

**Tech Stack:** {key technologies}

**Design:** [{NNNN} — {Title}]({design.dir}/{NNNN}-{date}-{slug}.md)

---
```

### The Skill Handles

The `superpowers:writing-plans` skill produces bite-sized tasks with:
- Exact file paths (Create/Modify/Test)
- Complete code in each step
- TDD: write failing test → implement → verify → commit
- Each step is one action (2-5 minutes)

Do NOT duplicate this — let the skill do its job.

---

## Step 3: Update README.md Indexes

### Design README (`{design.readme}`)

Insert at TOP of table (newest first):

```markdown
| Design | Description |
|--------|-------------|
| [{NNNN} — {Title}]({filename}) | {one-line description} |
```

### Plan README (`{plan.readme}`)

Insert at TOP of table (newest first):

```markdown
| Plan | Status | Description |
|------|--------|-------------|
| [{NNNN} — {Title}]({filename}) | Draft | {one-line description} |
```

---

## Step 4: Offer Execution

After both docs are written, present execution options:

```
Design: {design.dir}/{filename}
Plan:   {plan.dir}/{filename}

Execution options:

1. Subagent-Driven (this session)
   → superpowers:subagent-driven-development
   → Fresh subagent per task + two-stage review

2. Parallel Session (separate)
   → superpowers:executing-plans
   → Batch execution with checkpoints

3. Later — just commit the docs for now
```

If option 1: invoke `superpowers:subagent-driven-development`
If option 2: guide user to open new session with `superpowers:executing-plans`
If option 3: commit design + plan docs with `/dev-commit`

---

## Status Values

| Status | Meaning |
|--------|---------|
| Draft | Created, not yet reviewed |
| In Progress | Implementation started |
| Done | Implementation finished |
| Abandoned | Cancelled (document why) |

## Examples

```bash
/dev-plan add user authentication with OAuth
# → docs/design/0003-2026-02-18-user-auth.md
# → docs/plan/0003-2026-02-18-user-auth-impl.md

/dev-plan refactor test fixtures to use factory pattern
# → docs/design/0004-2026-02-18-test-fixture-refactor.md
# → docs/plan/0004-2026-02-18-test-fixture-refactor-impl.md
```
