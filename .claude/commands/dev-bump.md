---
version: 1.3.2
tier: agnostic
description: "Bump version and update changelog. Usage: /dev-bump <type> where type=patch|minor|major|tag"
---

# Version Bump and Release

You are performing a version bump and release.

## Step 0: Load Project Configuration

**FIRST**, read `project.yaml` from the repository root to get project-specific values:

```yaml
name: <project_name>
git:
  url: <git_url>
  default_branch: <branch>
version:
tier: agnostic
  file: <version_file>
  changelog: <changelog_file>
  sync_files:              # Optional: additional files to update version
    - package.json         # Root package.json
    - packages/*/package.json  # Workspace packages
```

Use these values throughout this command:
- `{project.name}` - Project name
- `{project.git.url}` - Git repository URL
- `{project.git.url}/commits/{hash}` - Commit URL pattern
- `{project.git.default_branch}` - Default branch for PR targets (e.g., main, develop)
- `{current_branch}` - Current working branch (get via `git branch --show-current`)
- `{project.version.file}` - Version file (e.g., VERSION)
- `{project.version.changelog}` - Changelog file (e.g., CHANGELOG.md)
- `{project.version.sync_files}` - Additional files to sync version (optional)

**Note**: For push commands, always use `{current_branch}` (the actual branch you're on), not `{project.git.default_branch}`.

## Parameters
- **Bump Type**: {{type}}
  - `patch`: Bug fixes and minor changes (0.1.0 → 0.1.1)
  - `minor`: New features, backwards compatible (0.1.0 → 0.2.0)
  - `major`: Breaking changes (0.1.0 → 1.0.0)
  - `tag`: Tag current version (first release - no version bump, just create tag and changelog)

## Pre-flight Check

**IMPORTANT**: Before proceeding, validate the bump type:

1. If `{{type}}` is empty, missing, or not one of `patch`, `minor`, `major`, `tag`:
   - Use `AskUserQuestion` tool to ask the user to select the bump type
   - Question: "Which version bump type would you like to perform?"
   - Options:
     - `tag` - Tag current version as first release (no bump)
     - `patch` - Bug fixes and minor changes (X.Y.Z → X.Y.Z+1)
     - `minor` - New features, backwards compatible (X.Y.Z → X.Y+1.0)
     - `major` - Breaking changes (X.Y.Z → X+1.0.0)
   - **DO NOT proceed** until a valid type is provided

2. Only proceed with the version bump after confirming a valid type.

3. **Special case for `tag`**:
   - If `{{type}}` is `tag`, skip version calculation
   - Use the current VERSION file value as-is
   - Useful for first release when VERSION is already set (e.g., 0.1.0)

## Steps to Execute

### 1. Get Current Version and Calculate New Version

Read `{project.version.file}` file to get the current version.

**If VERSION file exists:**
```
X.Y.Z
```

**If VERSION file does NOT exist (first release):**
- Treat current version as `0.0.0`
- This enables first tag creation:
  - `/dev-bump patch` → 0.0.1
  - `/dev-bump minor` → 0.1.0
  - `/dev-bump major` → 1.0.0

**For `tag` type (first release with existing VERSION):**
- Read the current version from VERSION file
- Use it directly as the release version (no calculation)
- Example: VERSION contains `0.1.0` → release as v0.1.0

Calculate new version based on bump type:
- **tag**: Use current version as-is (no bump)
- **patch**: Increment Z (X.Y.Z → X.Y.Z+1)
- **minor**: Increment Y, reset Z (X.Y.Z → X.Y+1.0)
- **major**: Increment X, reset Y and Z (X.Y.Z → X+1.0.0)

### 2. Get Commits Since Last Release

Run the following commands to get commits:
```bash
# Get the last version tag
git describe --tags --abbrev=0 2>/dev/null || echo "(none)"

# Get commits since last tag (or all commits if no tag)
git log <last_tag>..HEAD --oneline --no-merges

# If no previous tag, get ALL commits
git log --oneline --no-merges
```

**First release**: If no previous tag exists, include ALL commits in the changelog.

### 3. Categorize Commits

Analyze each commit message and categorize into:

**Features** (Added):
- Commits starting with `feat:` or `feat(` or containing "Add" in the first line
- Example: `Add user authentication flow`

**Bug Fixes** (Fixed):
- Commits starting with `fix:` or `fix(` or containing "Fix" in the first line
- Example: `Fix config file parsing for nested keys`

**Improvements** (Changed):
- Commits starting with `chore:`, `refactor:`, `perf:`, `style:`, `test:`, `build:`, `ci:`
- Commits containing "Improve", "Update", "Refine", "Rename"
- Example: `Improve error handling in retry logic`

**Documentation** (Changed - Documentation):
- Commits starting with `docs:` or `docs(`
- Commits only updating documentation files
- Example: `docs: update README`

### 4. Update VERSION File

**Skip this step if `{{type}}` is `tag`** (VERSION already has the correct value).

For other types, write the new version to `{project.version.file}` file:
```
NEW_VERSION
```

### 5. Update Additional Version Files (sync_files)

**Skip this step if `{{type}}` is `tag`**.

If `{project.version.sync_files}` is defined in project.yaml, update version in each file:

**For package.json** (Node.js/Bun projects):
Use the Edit tool to update the `version` field:
```json
"version": "NEW_VERSION",
```

**For pyproject.toml** (Python projects):
```bash
sed -i 's/^version = .*/version = "NEW_VERSION"/' pyproject.toml
```

**For Cargo.toml** (Rust projects):
```bash
sed -i 's/^version = .*/version = "NEW_VERSION"/' Cargo.toml
```

**Note**: `bun.lockb` and `package-lock.json` do NOT need updating - they only track dependency versions.

Skip this step if `sync_files` is not defined or empty.

### 6. Sync Lock Files

**Skip this step if `{{type}}` is `tag`**.

After updating version in sync_files, regenerate lock files so they stay in sync:

| sync_file        | Lock file           | Command                               |
| ---------------- | ------------------- | ------------------------------------- |
| `pyproject.toml` | `uv.lock`           | `uv lock`                             |
| `package.json`   | `package-lock.json` | `npm install --package-lock-only`     |
| `package.json`   | `bun.lockb`         | `bun install --frozen-lockfile=false` |
| `Cargo.toml`     | `Cargo.lock`        | `cargo generate-lockfile`             |

Only run the command if the corresponding lock file already exists in the repo. Include the lock file in the commit (Step 8).

### 7. Update or Create CHANGELOG.md

If `{project.version.changelog}` doesn't exist, create it. Otherwise, update it.

Format the changelog following this structure (replace `{project.git.url}` with actual URL from project.yaml):

```markdown
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Future features go here

### Changed
- N/A

### Fixed
- N/A

---

## [NEW_VERSION] - YYYY-MM-DD

### Added
- [hash1]({project.git.url}/commits/hash1) description
- [hash2]({project.git.url}/commits/hash2) description

### Changed
- [hash3]({project.git.url}/commits/hash3) description
- [hash4]({project.git.url}/commits/hash4) description

### Fixed
- [hash5]({project.git.url}/commits/hash5) description

---

## [PREVIOUS_VERSION] - YYYY-MM-DD
...

---

[Unreleased]: {project.git.url}/compare/vNEW_VERSION...HEAD
[NEW_VERSION]: {project.git.url}/commits?until=refs%2Ftags%2FvNEW_VERSION
[PREVIOUS_VERSION]: {project.git.url}/commits?until=refs%2Ftags%2FvPREVIOUS_VERSION
```

**Rules for CHANGELOG.md**:
1. New version section goes ABOVE older versions
2. Use today's date in YYYY-MM-DD format
3. Each commit includes 7-character hash and description
4. Commits are grouped by type: Added, Changed, Fixed, Security
5. Clean up commit messages (remove emoji, "Generated with Claude Code" footer)
6. Link format: `[hash](url) description`
7. Update comparison links at the bottom
8. Skip empty sections (if no Added/Changed/Fixed/Security, don't include the section)
9. **For first tag (no previous tag)**: Only include link for the version itself, not a comparison link. Example:
   ```
   [0.1.0]: {project.git.url}/commits?until=refs%2Ftags%2Fv0.1.0
   ```

### 8. Commit Changes

**For `tag` type** (only CHANGELOG.md is new):
```bash
git add {project.version.changelog}
git commit -m "chore: release vCURRENT_VERSION

- Add {project.version.changelog} with release notes
- Tag current version as first release"
```

**For other types** (VERSION, sync_files, lock files, and CHANGELOG.md changed):
```bash
git add {project.version.file} {project.version.changelog} [sync_files...] [lock_files...]
git commit -m "bump: vNEW_VERSION

- Update {project.version.file} to NEW_VERSION
- Update [sync_files] version to NEW_VERSION (if applicable)
- Sync lock files (if applicable)
- Update {project.version.changelog} with release notes"
```

### 9. Create Git Tag

Create an annotated tag:
```bash
git tag -a vNEW_VERSION -m "Release vNEW_VERSION"
```

### 10. Provide Push Instructions

First, get the current branch name:
```bash
git branch --show-current
```

Output the following instructions (replace placeholders with actual values, use current branch for push):

**For `tag` type:**
```
Released vCURRENT_VERSION

Summary:
- Created: {project.version.changelog}
- Committed: Release commit
- Tagged: vCURRENT_VERSION
```

**For other types:**
```
Version bumped to vNEW_VERSION

Summary:
- Updated: {project.version.file}, [sync_files if any], [lock files if any], {project.version.changelog}
- Committed: Version bump commit
- Tagged: vNEW_VERSION

To push to remote:

  # Push commits and tags together
  git push origin {current_branch} --follow-tags

  # Or push separately
  git push origin {current_branch}
  git push origin vNEW_VERSION

View release:
  {project.git.url}/commits?until=refs%2Ftags%2FvNEW_VERSION
```

## Important Notes

- **DO NOT push** automatically - let the user decide when to push
- **Validate** that all files are updated before committing
- **Check** that the new version follows semantic versioning
- **Ensure** all commit hashes are correct and links work
- **Verify** the changelog is properly formatted
- **Clean** commit messages: remove emojis and Claude Code footers for changelog entries

## Example Execution

### First Release (VERSION already set, no tags)

For `/dev-bump tag` (recommended when VERSION already exists):
- Current VERSION: 0.1.0
- Release: v0.1.0 (no bump, use current version)
- Changelog: Lists ALL commits (creates CHANGELOG.md)
- Tag: v0.1.0
- **No changes to VERSION or sync_files**

### First Release (no VERSION file, no tags)

For `/dev-bump minor` (recommended for first release from scratch):
- Current: (none) → treated as 0.0.0
- New: 0.1.0
- Changelog: Lists ALL commits (creates CHANGELOG.md)
- Tag: v0.1.0

For `/dev-bump patch`:
- Current: (none) → treated as 0.0.0
- New: 0.0.1
- Tag: v0.0.1

For `/dev-bump major`:
- Current: (none) → treated as 0.0.0
- New: 1.0.0
- Tag: v1.0.0

### Subsequent Releases

For `/dev-bump patch`:
- Current: 0.1.0
- New: 0.1.1
- Changelog: Lists commits since v0.1.0
- Tag: v0.1.1

For `/dev-bump minor`:
- Current: 0.1.0
- New: 0.2.0
- Changelog: Lists commits since v0.1.0
- Tag: v0.2.0

For `/dev-bump major`:
- Current: 0.1.0
- New: 1.0.0
- Changelog: Lists commits since v0.1.0
- Tag: v1.0.0
