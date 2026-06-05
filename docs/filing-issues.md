# Filing issues (and how they're triaged)

Issues in this repo can be picked up **automatically** by the dev agent
(**wiwi**) and shipped through review without a human writing the first
draft — but only if the issue passes a strict **triage gate** first. This
doc explains the flow and, more usefully, **how to write an issue that an
agent (or a human) can actually act on**.

If you're a human filing an issue, the short version is: *be concrete, scope
it small, and name the test that proves it's done.* If you're an **agent**
filing issues programmatically (a monitor, a planner, another coding agent),
the same rules apply — plus one scrubbing rule at the end.

## The flow

```
you file an issue
     │   add label: agent:assess        ← this is what routes it into the loop
     ▼
triage agent ── 5 gates ──►  verdict: do | needs_info | skip   (posts a comment)
     │  do → adds label: agent:try
     ▼
wiwi (dev agent) ──► branch off main · implement · cargo build + tests green
     │                                   └─► opens a DRAFT PR (label: auto-agent)
     ▼
CI ──► vivi (review agent) ──► structured review ──► gated auto-merge
```

- **Routing.** An issue does nothing automatic until it carries the
  `agent:assess` label. Add it when the issue is ready to be assessed; leave
  it off for discussion/parking. (Filing alone never triggers an agent.)
- **Triage** actually investigates every issue — reads the relevant code and
  reproduces it where it can — then **replies in a maintainer's voice** before
  running five gates to decide the verdict. The gates decide *only* whether the
  dev agent can safely implement it unattended; they are **not** a judgement on
  whether the issue is worth doing. Every reporter gets a warm, investigated
  reply **in the language they filed the issue in**, whatever the verdict. On
  `do` it also labels `agent:try`, which starts **wiwi**.
- **wiwi** is allowed to open a PR only if `cargo build` and the tests pass;
  otherwise it aborts and explains why on the issue. The PR is a **draft**
  labelled `auto-agent`.
- **vivi** reviews every PR after CI; auto-merge is gated (see
  [pr-review-agent.md](pr-review-agent.md)).

The gates are enforced by
[`scripts/agent-bot/run_triage.sh`](../scripts/agent-bot/run_triage.sh) —
that script is the single source of truth; the list below mirrors it.

## The 5 gates

An issue is judged **auto-implementable by an unattended agent** only when
**all five** pass:

1. **Concrete + checkable.** It has a concrete, actionable description **and**
   explicit acceptance criteria — you can list **2+ assertions** that are
   objectively checkable.
2. **Small.** Estimated diff **< 300 lines** across **< 10 files**.
3. **Contained.** The change stays within **`console/`, `docs/`, one crate,
   or one workflow** — not cross-cutting architecture work.
4. **No new surface.** **No** new runtime dependency, **no** new secret, **no**
   new external network call.
5. **Deterministically testable.** The fix ships with a deterministic test
   (unit / integration / `cargo check`) **in the same PR** — not "needs manual
   QA".

When in doubt, triage is **strict** — it would rather defer than guess.

## The three verdicts — and what to do next

| Verdict | Means | What you do |
|---|---|---|
| **`do`** | all 5 gates pass | nothing — triage adds `agent:try`, **wiwi** starts |
| **`needs_info`** | gate 1 failed (vague / no acceptance criteria) | edit the issue to add specifics + criteria, then re-add `agent:assess` to re-triage |
| **`skip`** | one of gates 2–5 failed (too big / cross-cutting / new dep / not deterministically testable) | this is **human / collaborative** work — see below |

A `skip` isn't a rejection of the *idea*; it's a statement that the work is
too big or too cross-cutting to hand to an unattended agent. You have two
overrides, both manual labels:

- **`agent:try`** — force wiwi to attempt it anyway (use when you're confident
  it's safe despite the gate).
- **`agent:skip`** — mute future re-triage (use when it's intentionally a
  human task and you don't want triage re-commenting).

> **Real example.** The body-cap feature (cap stored request/response bodies
> for 1M-token contexts) was triaged **`skip`** — it failed gate 3 by spanning
> three crates plus a storage migration. It was done as a human-directed
> collaboration instead. That's the gate working as intended: *it doesn't make
> agents do more, it makes them only do what they can do well.*

## How to write a gate-passing issue

Structure beats prose. A good issue reads like a spec the agent (or a
teammate) can execute without guessing:

- **Title** — imperative and scoped: *"cap stored bodies"*, not *"bodies are
  too big sometimes?"*.
- **Goal** — one or two sentences: what changes and why.
- **Why now** — the trigger (optional but helps prioritisation).
- **In scope / Out of scope** — draw the box explicitly. Naming what's *out*
  is what keeps a change inside gate 3.
- **Acceptance criteria** — a checkbox list of objectively checkable
  assertions. This is gate 1 **and** gate 5 in one move.
- **Suggested approach** — point at the function/module; saves the agent a
  discovery pass.
- **Files / crates touched** — keeps you (and the reviewer) honest about gate
  2 and gate 3.

Two anti-patterns that turn a `do` into a `skip`:

- **Bundling.** "While we're in there, also refactor X" — split it; one issue,
  one contained change.
- **"We'll QA it manually."** If you can't name a deterministic test, the
  change isn't agent-ready (gate 5) — and arguably isn't *done*-able either.

### Copy-paste template

```markdown
## Goal
<one or two sentences: what changes and why>

## Why now
<the trigger — optional>

## In scope
- <bullet>

## Out of scope (explicit)
- <bullet — what this issue deliberately does NOT touch>

## Acceptance criteria
- [ ] <objectively checkable assertion #1>
- [ ] <objectively checkable assertion #2>

## Suggested approach
<which function / module; the cheapest correct path>

## Files / crates touched
- <path or crate>
```

When the issue is ready, add the **`agent:assess`** label to route it into
triage.

## If your agent files issues

Monitors and planners can open issues too — for example the production
observer **mara** files incidents automatically, and a teammate routes the
useful ones into the loop by adding `agent:assess`. If you wire up an agent
that opens issues:

- **Use the same structure** above — an issue with explicit acceptance
  criteria is far more likely to clear triage than a wall of logs.
- **Add `agent:assess`** only when the issue is genuinely ready to be worked,
  not for every alert.
- **Scrub internal infrastructure** out of the body before it's filed: no
  private IPs, internal hostnames, credentials, or machine-specific paths.
  This repo enforces that rule in CI
  ([`scripts/lint/check-leakage.sh`](../scripts/lint/check-leakage.sh)), and
  mara masks IPs and home paths before it files — hold your own agent to the
  same bar.

## See also

- [pr-review-agent.md](pr-review-agent.md) — how PRs are reviewed (**vivi**)
  and the conditions under which an agent PR is auto-merged.
- [`scripts/agent-bot/run_triage.sh`](../scripts/agent-bot/run_triage.sh) —
  the triage gates, verbatim (source of truth for this doc).
