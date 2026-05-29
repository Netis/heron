---
name: dev-bump
version: 0.2.0
description: "Bump Heron version via the VERSION-file SSOT. Delegates to `just bump` which syncs server/Cargo.toml (workspace.package.version) and console/package.json. Also updates CHANGELOG.md and creates a bump commit + tag. Usage: /dev-bump <type> where type=patch|minor|major|tag"
---

# dev-bump — Version bump for Heron

Heron uses a **VERSION-file SSOT** pattern (see Core Principles → Single Source of Truth in AGENTS.md):

| File | Role |
|---|---|
| `VERSION` (repo root) | **Canonical.** Plain `X.Y.Z` + trailing newline. Every other version reference derives from here. |
| `server/Cargo.toml` (`workspace.package.version`) | Derived — rewritten by `just bump`. Cargo requires a literal version string. |
| `console/package.json` (`version`) | Derived — rewritten by `just bump`. npm tooling needs the literal. |
| `server/ts-common/src/version.rs` | Reads VERSION via `include_str!` at compile time. Other Rust crates call `ts_common::version::version()`. |
| `console/vite.config.ts` | Reads VERSION at build time, exposes as `__APP_VERSION__` for frontend code. |

## Parameters

- **type** — `patch` | `minor` | `major` | `tag` | `set X.Y.Z`
  - `patch`: `0.1.0 → 0.1.1`
  - `minor`: `0.1.0 → 0.2.0`
  - `major`: `0.1.0 → 1.0.0`
  - `tag`: no version change, just create `v<current>` git tag (for retagging or after a manual bump)
  - `set X.Y.Z`: set exact version (pre-release coordination, reverts)

## Workflow

### 1. Pre-flight

```bash
just bump check     # verifies VERSION == Cargo.toml == package.json
git status          # must be clean (or ask user to stash/commit first)
```

If `just bump check` shows drift, fix it before bumping: run `just bump set <VERSION-value>` to force Cargo.toml / package.json back in sync with VERSION (or vice versa — decide with the user which value is authoritative).

### 2. Bump

```bash
just bump <type>    # patch | minor | major | set X.Y.Z
```

This rewrites `VERSION`, `server/Cargo.toml`, `console/package.json` atomically. Verify:

```bash
just bump check
```

### 3. Update CHANGELOG.md

Create or update `CHANGELOG.md` at repo root using Keep-a-Changelog format:

```markdown
## [0.2.0] — 2026-04-15

### Added
- ...

### Changed
- ...

### Fixed
- ...
```

Source entries from `git log v<previous>..HEAD --oneline`. Group by conventional-commit type (`feat` → Added, `fix` → Fixed, `refactor`/`chore` → Changed). Skip trivial commits (pure docs/test unless significant).

### 4. Commit and tag

```bash
git add VERSION server/Cargo.toml console/package.json CHANGELOG.md
git commit -m "bump: v<new-version>"
git tag v<new-version>
```

**Do not push automatically.** Report the new tag back to the user; they decide when to push.

### 5. Sanity build (optional but recommended)

If the bump touches Rust or TS:

```bash
just quality all
```

Catches the case where `include_str!` path drift or `__APP_VERSION__` typing breaks after a refactor.

## Type: `tag` (no version change)

For retagging the current VERSION value without changing it (e.g. after a manual VERSION edit, or re-tagging from a different commit):

```bash
current="$(cat VERSION | tr -d '[:space:]')"
git tag "v$current"
```

Still update CHANGELOG if release notes changed.

## Guardrails

- **Never edit Cargo.toml / package.json version fields directly.** If you're tempted, it means `just bump` is broken — fix the script, not the derived files.
- **Never commit a drifted state.** `just bump check` should be green before every bump commit.
- **Never push tags automatically.** User decides.
- **Never rename `VERSION`.** It is referenced by path from `vite.config.ts`, `ts-common/src/version.rs`, `scripts/routers/shared/bump.sh`, and `justfile`.

## Output format after a run

Report:
1. Old → new version
2. `just bump check` result (should be green)
3. Commit hash and tag name created
4. Reminder: user must `git push && git push --tags` when ready
