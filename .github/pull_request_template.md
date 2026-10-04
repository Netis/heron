<!--
Keep PRs small and single-purpose. Scrub all internal infra details before
submitting (private IPs, hostnames, machine paths, team/app names) — see the
PR-hygiene rule in CLAUDE.md.
-->

## What

<!-- One or two sentences: the user-visible change. -->

## Why

<!-- The problem / failure this prevents, or the issue it closes. -->

## Checklist

- [ ] `cargo test --workspace` passes (`bun test` under `console/` if touched).
- [ ] **Critical behavior is covered.** If you touched a **Tier 0/1 module**
      (`verification/coverage-policy.json`) or added a critical behavior, add or
      update a scenario in `verification/scenarios.json` and tag its test with
      `// @scenario <ID> <kind>`, `kind ∈ {unit, integration, e2e, fault}`.
      `python3 scripts/ci/check_scenarios.py` must report `N defined, M tag(s), OK`.
      (A P0 scenario must declare at least one behavior-level kind — not unit
      only. See `verification/README.md` → "Adding a scenario".)
- [ ] Changed lines stay covered: `coverage.yml` enforces changed-code coverage
      and the no-decrease ratchet against `verification/coverage-baseline.json`.
- [ ] **Formatting:** I did **not** run crate-/repo-wide `cargo fmt`. The
      committed tree was formatted with an older rustfmt, so the current pinned
      nightly rewrites untouched files and CI does not gate formatting — format
      only the hunks this PR touches.
- [ ] No internal infra details leaked (private IPs, hostnames, machine paths,
      team/app names) — see the PR-hygiene rule in CLAUDE.md.

## Notes for reviewers

<!-- Optional: risky areas, follow-ups, or how to verify by hand. -->
