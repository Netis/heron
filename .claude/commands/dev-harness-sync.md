---
version: 1.0.1
tier: agnostic
description: "Sync the full Claude Code harness for this project (hooks, guards, settings, plugins, permissions, .gitignore, project.yaml, CLAUDE.md). Usage: /dev-harness-sync"
---

# /dev-harness-sync

Sync the complete Claude Code harness for the current project. Covers hooks, guards, plugin
allowlist, project.yaml reconciliation, submodule auto-add, .gitignore patterns, and the
CLAUDE.md core block — all in a single idempotent command. This command supersedes and absorbs
`/dev-project-sync`.

> **Hard boundary**: This command does NOT touch `justfile`, `scripts/routers/`, `commands/`,
> or `skills/`. Syncing those components is exclusively `/dev-target merge`'s responsibility.

---

## Prerequisites

`jq` (JSON processor) and `yq` (Mike Farah's Go implementation — not the Python one) must be
installed.

```bash
# macOS
brew install jq yq

# Linux (Debian/Ubuntu)
sudo apt-get install jq
sudo wget -qO /usr/local/bin/yq https://github.com/mikefarah/yq/releases/latest/download/yq_linux_amd64
sudo chmod +x /usr/local/bin/yq

# Verify
jq --version && yq --version
```

Script invocations below assume you are running from inside the target project itself.
All paths are relative to the project root (CWD).

---

## Phase 1 — Inspect

Read current state before making any changes.

### 1.1 Read project files

```bash
# CC toolkit directory
ls .claude/ 2>/dev/null || echo ".claude/ missing"

# Settings files
cat .claude/settings.local.json 2>/dev/null || echo "{}"
cat ~/.claude/settings.json | jq '{enabledPlugins, hooks, permissions}' 2>/dev/null

# Config files
test -f project.yaml && cat project.yaml || echo "project.yaml missing"
test -f CLAUDE.md && echo "CLAUDE.md present" || echo "CLAUDE.md missing"
test -f .gitignore && cat .gitignore || echo ".gitignore missing"
```

### 1.2 Detect project runtime

Infer language and runtime from file indicators (in priority order):

| File present | Language | Runtime |
|---|---|---|
| `pyproject.toml` | python | uv (if `uv.lock` present) else pip |
| `tsconfig.json` | typescript | bun (if `bun.lockb`) else npm |
| `package.json` (no tsconfig) | javascript | bun (if `bun.lockb`) else npm |
| `Cargo.toml` | rust | cargo |
| `go.mod` | go | go |
| `pom.xml` / `build.gradle` | java | maven/gradle |
| `Package.swift` | swift | swift |
| `CMakeLists.txt` | c | cmake |

Cross-check with `type:` and `runtime:` fields in `project.yaml` if present. File indicators
win on conflict.

### 1.3 List installed plugins

```bash
cat ~/.claude/plugins/installed_plugins.json 2>/dev/null | jq 'keys' || echo "No plugin registry found"
```

Use this for reference when building the allowlist in Phase 2.

---

## Phase 2 — Plugin Allowlist

Every project MUST declare every known plugin explicitly as `true` or `false` in
`.claude/settings.local.json`. Global state drifts and is shared across all projects; local
state is the contract for this project. Do not rely on inheritance from `~/.claude/settings.json`.

### Plugin decision matrix

| Plugin | Default | Condition / Notes |
|---|---|---|
| `superpowers@claude-plugins-official` | **always true** | Universal baseline |
| `skill-creator@claude-plugins-official` | **always true** | Universal baseline |
| `claude-md-management@claude-plugins-official` | **always true** | Universal baseline |
| `claude-code-setup@claude-plugins-official` | **always true** | Universal baseline |
| `security-guidance@claude-plugins-official` | **always true** | Universal baseline |
| `codex@openai-codex` | **always true** | Universal baseline |
| `pyright-lsp@claude-plugins-official` | conditional | true if python; else false |
| `typescript-lsp@claude-plugins-official` | conditional | true if typescript or javascript; else false |
| `gopls-lsp@claude-plugins-official` | conditional | true if go; else false |
| `rust-analyzer-lsp@claude-plugins-official` | conditional | true if rust; else false |
| `jdtls-lsp@claude-plugins-official` | conditional | true if java; else false |
| `swift-lsp@claude-plugins-official` | conditional | true if swift; else false |
| `clangd-lsp@claude-plugins-official` | conditional | true if c/c++; else false |
| `frontend-design@claude-plugins-official` | **never** | Only for TS/JS frontend projects with explicit UI focus |
| `playwright@claude-plugins-official` | **never** | Only for E2E/UI testing projects |
| `plugin-dev@claude-plugins-official` | **never** | Only for plugin/skill authoring repos |
| `agent-sdk-dev@claude-plugins-official` | **never** | Only for Agent SDK apps |
| `playground@claude-plugins-official` | **never** | Only for prototyping/demo projects |
| `pr-review-toolkit@claude-plugins-official` | **never** | Toolkit ships dev-review skill; redundant |
| `code-review@claude-plugins-official` | **never** | Toolkit ships dev-review skill; redundant |
| `code-simplifier@claude-plugins-official` | **never** | Toolkit ships dev-review skill; redundant |
| `feature-dev@claude-plugins-official` | **never** | Toolkit ships dev-plan command; redundant |
| `slack@claude-plugins-official` | **never** | Enable only when Slack API actually used |
| `vercel@claude-plugins-official` | **never** | Enable only when Vercel deployment used |
| `supabase@claude-plugins-official` | **never** | Enable only when Supabase backend used |
| `stripe@claude-plugins-official` | **never** | Enable only when Stripe payments used |
| `firecrawl@claude-plugins-official` | **never** | Enable only when web scraping needed |
| `obsidian@obsidian-skills` | **never** | Enable only when Obsidian vault integration needed |

### Apply allowlist via script

```bash
bash scripts/lib/harness/plugin_allowlist.sh <project_type> .claude/settings.local.json
```

### Fallback inline jq (if script missing)

Substitute the detected project type for `<TYPE>`. Set LSP booleans accordingly.

```bash
jq '
  .enabledPlugins["superpowers@claude-plugins-official"] = true
  | .enabledPlugins["skill-creator@claude-plugins-official"] = true
  | .enabledPlugins["claude-md-management@claude-plugins-official"] = true
  | .enabledPlugins["claude-code-setup@claude-plugins-official"] = true
  | .enabledPlugins["security-guidance@claude-plugins-official"] = true
  | .enabledPlugins["codex@openai-codex"] = true
  | .enabledPlugins["pyright-lsp@claude-plugins-official"] = false
  | .enabledPlugins["typescript-lsp@claude-plugins-official"] = false
  | .enabledPlugins["gopls-lsp@claude-plugins-official"] = false
  | .enabledPlugins["rust-analyzer-lsp@claude-plugins-official"] = false
  | .enabledPlugins["jdtls-lsp@claude-plugins-official"] = false
  | .enabledPlugins["swift-lsp@claude-plugins-official"] = false
  | .enabledPlugins["clangd-lsp@claude-plugins-official"] = false
  | .enabledPlugins["plugin-dev@claude-plugins-official"] = false
  | .enabledPlugins["agent-sdk-dev@claude-plugins-official"] = false
  | .enabledPlugins["frontend-design@claude-plugins-official"] = false
  | .enabledPlugins["playwright@claude-plugins-official"] = false
  | .enabledPlugins["playground@claude-plugins-official"] = false
  | .enabledPlugins["pr-review-toolkit@claude-plugins-official"] = false
  | .enabledPlugins["code-review@claude-plugins-official"] = false
  | .enabledPlugins["code-simplifier@claude-plugins-official"] = false
  | .enabledPlugins["feature-dev@claude-plugins-official"] = false
  | .enabledPlugins["slack@claude-plugins-official"] = false
  | .enabledPlugins["vercel@claude-plugins-official"] = false
  | .enabledPlugins["supabase@claude-plugins-official"] = false
  | .enabledPlugins["stripe@claude-plugins-official"] = false
  | .enabledPlugins["firecrawl@claude-plugins-official"] = false
  | .enabledPlugins["obsidian@obsidian-skills"] = false
  | .includeCoAuthoredBy //= false
' .claude/settings.local.json > /tmp/settings_patched.json \
  && mv /tmp/settings_patched.json .claude/settings.local.json
```

Adjust LSP `true`/`false` values per the decision matrix for the detected project type before
running. After applying, validate:

```bash
jq . .claude/settings.local.json > /dev/null && echo "Valid JSON"
```

---

## Phase 3 — project.yaml Reconciliation

`project.yaml` is the adaptation layer between canonical commands and the project. Fields are
classified as auto-derivable, default-safe, or sacred (user-owned).

**Sacredness rule**: only touch a field if it is listed below AND classified `auto` or `default`
AND the field is currently empty, absent, or demonstrably stale. Everything else is sacred.

### Field schema

| Field | Default | Classification | Notes |
|---|---|---|---|
| `name` | repo dir name | auto | Derive from `git remote get-url origin` or dir name |
| `git.url` | from remote | auto | Parse `git remote get-url origin` |
| `git.default_branch` | `main` | default | Set only if absent |
| `version.file` | `VERSION` | default | Set only if absent |
| `version.changelog` | `CHANGELOG.md` | default | Set only if absent |
| `version.sync_files` | `[]` | default | Leave empty unless hardcoded version strings found |
| `type` | detected | auto | Derived from file indicators (Phase 1.2) |
| `runtime` | detected | auto | Derived from lockfile indicators (Phase 1.2) |
| `submodules` | `[]` | auto | Populated by Phase 4; do not hand-edit |
| `workspace.file` | — | sacred | User owns workspace manifest path |
| `code_review` | — | sacred | User owns code review config entirely |
| `bug.dir` | `docs/bug` | default | Set only if absent and `bug:` section exists |
| `bug.readme` | `docs/bug/README.md` | default | Set only if absent and `bug:` section exists |
| `research.dir` | `docs/research` | default | Set only if absent |
| `design.dir` | `docs/design` | default | Set only if absent |
| `plan.dir` | `docs/plan` | default | Set only if absent |

### Apply reconciliation via script

```bash
bash scripts/lib/harness/project_yaml_recon.sh .
```

The script reads the current `project.yaml`, applies auto/default fields where missing, and
writes the result back. It never removes existing keys.

---

## Phase 4 — Submodule Auto-Add

Detect Git submodules that exist on disk but are missing from `project.yaml`'s `submodules:`
list.

**Rules**:
- Additive only. Existing entries are never modified or removed.
- Submodules are identified by `.gitmodules` entries.
- Stale entries (path registered but directory missing) are flagged in the report but not
  removed — engineer must confirm before deletion.

```bash
bash scripts/lib/harness/submodule_autoadd.sh .
```

---

## Phase 5 — .gitignore Ensure

Ensure required patterns are present in `.gitignore`. Never rewrite or reorder existing
content — only append missing lines.

Required patterns:

```
.claude/*.local.*
.obsidian/
repos/
repos
.superset/
repos-meta/
repos-meta
```

```bash
bash scripts/lib/harness/gitignore_ensure.sh .
```

---

## Phase 6 — Hooks & Guards

Install and validate harness hooks — both global machine-level and project-local.

### 6.1 Check canonical sources

```bash
ls scripts/hooks/*.sh
```

List all hook scripts in the toolkit source. These are the source of truth.

### 6.2 Check current state

```bash
# Global hooks
ls ~/.claude/hooks/ 2>/dev/null

# Global settings — existing PreToolUse hooks
cat ~/.claude/settings.json | jq '.hooks.PreToolUse // []' 2>/dev/null

# Local settings — existing hooks
cat .claude/settings.local.json 2>/dev/null | jq '.hooks // {}'
```

### 6.3 Sync global hooks

```bash
mkdir -p ~/.claude/hooks
cp scripts/hooks/*.sh ~/.claude/hooks/
chmod +x ~/.claude/hooks/*.sh
```

### 6.4 Ensure global PreToolUse hook

Check `~/.claude/settings.json` for a PreToolUse hook that runs `guard-dangerous-commands.sh`.

```bash
# Count existing PreToolUse Bash matchers
jq '.hooks.PreToolUse // [] | map(select(.matcher == "Bash")) | length' ~/.claude/settings.json
```

If result is `0`, merge the following into `~/.claude/settings.json` — do NOT overwrite other
hook entries (e.g. notification hooks):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/.claude/hooks/guard-dangerous-commands.sh"
          }
        ]
      }
    ]
  }
}
```

If result is `> 0`, verify it already references `guard-dangerous-commands.sh`. If the
reference exists, skip. Otherwise add it alongside existing matchers.

### 6.5 Ensure project-local hooks in settings.local.json

Check `.claude/settings.local.json` for these project-specific hooks. Create the file if it
does not exist. Only update the `hooks` section — preserve `enabledPlugins` and all other keys.

**PreToolUse — Dangerous Command Guard** (matcher: `Bash`):
Inline guard blocking destructive operations for portability when global hook is not installed.

**PreToolUse — Submodule Edit Guard** (matcher: `Edit|Write`) — workspace projects only:
Block accidental edits to files inside registered submodule directories.

**PostToolUse — Submodule Pointer Drift** (matcher: `Bash`) — workspace projects only:
Warn when submodule pointers go out of sync after shell commands.

### 6.6 Ensure MCP server: context7

```bash
claude mcp list 2>/dev/null | grep -q 'context7' && echo "context7 present" || echo "context7 MISSING"
```

If missing:

```bash
claude mcp add context7 -- npx -y @upstash/context7-mcp@latest
```

### 6.7 Validate hooks

```bash
# Global guard executable
test -x ~/.claude/hooks/guard-dangerous-commands.sh && echo "guard: ok" || echo "guard: MISSING"

# Global hook registered
jq '.hooks.PreToolUse // []' ~/.claude/settings.json | grep -q 'guard-dangerous' \
  && echo "global hook: ok" || echo "global hook: NOT REGISTERED"

# Local hooks present
jq '.hooks // {}' .claude/settings.local.json | grep -q 'PreToolUse' \
  && echo "local hook: ok" || echo "local hook: NOT SET"

# Functional test
echo '{"tool_name":"Bash","tool_input":{"command":"git push --force"}}' \
  | TOOL_INPUT='{"command":"git push --force"}' \
    bash ~/.claude/hooks/guard-dangerous-commands.sh 2>&1 \
  | grep -q 'BLOCK' && echo "guard blocks force push: PASS" || echo "guard blocks force push: FAIL"

echo '{"tool_name":"Bash","tool_input":{"command":"git status"}}' \
  | TOOL_INPUT='{"command":"git status"}' \
    bash ~/.claude/hooks/guard-dangerous-commands.sh 2>&1 \
  | grep -q 'BLOCK' \
  && echo "guard allows safe commands: FAIL" || echo "guard allows safe commands: PASS"
```

---

## Phase 7 — CLAUDE.md Core Block

Ensure `CLAUDE.md` contains the standard Core Principles block. Uses a sentinel comment for
idempotency — the block is inserted at most once.

**Sentinel**: `<!-- dev-harness-sync: core-block-v1 -->`

### 7.1 Check for existing block

```bash
grep -q 'dev-harness-sync: core-block-v1' CLAUDE.md 2>/dev/null \
  && echo "Core block present — skip" || echo "Core block MISSING — will insert"
```

### 7.2 Insert core block if missing

If the sentinel is absent, append the following block to `CLAUDE.md` (or create `CLAUDE.md`
if it does not exist):

```markdown
<!-- dev-harness-sync: core-block-v1 -->
## Core Principles

1. **Occam's Razor** — prefer the simplest solution that works.
2. **Code Quality** — readable, maintainable, well-structured over clever or over-engineered.
3. **Documentation** — keep docs in sync with code. CLAUDE.md reflects what is actually true.
4. **Project Structure** — follow the conventions established in this project. Check project.yaml for authoritative paths and config.
5. **Explicit over Implicit** — clear code over magic. Name things for what they do.
```

Preserve all existing content. Only append — do not reorder or reformat existing sections.

---

## Phase 8 — MCP Permissions Cleanup

Remove dangling `mcp__<plugin>__*` entries from `permissions.allow` in
`.claude/settings.local.json` whose plugin is set to `false` in `enabledPlugins`.

**Engineer note**: Test carefully before applying. Inspect the diff before writing.

### 8.1 Identify dangling MCP permissions

```bash
# Extract false plugins
FALSE_PLUGINS=$(jq -r '.enabledPlugins | to_entries | .[] | select(.value == false) | .key | split("@")[0]' \
  .claude/settings.local.json)

# Show which permissions.allow entries match disabled plugins
jq --arg plugins "$FALSE_PLUGINS" '
  .permissions.allow // [] |
  map(select(startswith("mcp__"))) |
  map(. as $p |
    ($p | ltrimstr("mcp__") | split("__")[0]) as $plugin_name |
    select($plugins | split("\n") | any(. == $plugin_name)) |
    $p
  )
' .claude/settings.local.json
```

### 8.2 Remove dangling entries

```bash
# Build regex of disabled LSP plugin names for filtering
jq '
  (.enabledPlugins | to_entries | map(select(.value == false) | .key | split("@")[0]) | join("|")) as $disabled |
  .permissions.allow //= [] |
  .permissions.allow |= map(
    select(
      (startswith("mcp__") | not) or
      (ltrimstr("mcp__") | split("__")[0] | IN($disabled | split("|")[])) | not
    )
  )
' .claude/settings.local.json > /tmp/settings_cleaned.json
```

Review `/tmp/settings_cleaned.json` before moving:

```bash
diff <(jq . .claude/settings.local.json) <(jq . /tmp/settings_cleaned.json)
```

If the diff looks correct:

```bash
mv /tmp/settings_cleaned.json .claude/settings.local.json
```

---

## Phase 9 — Validate & Report

### 9.1 Validate JSON/YAML files

```bash
jq . .claude/settings.local.json > /dev/null && echo "settings.local.json: valid JSON"
jq . ~/.claude/settings.json > /dev/null && echo "settings.json: valid JSON"
test -f project.yaml && yq . project.yaml > /dev/null && echo "project.yaml: valid YAML"
```

### 9.2 Print concise report

```
Harness Sync Report
===================
Project: {name} ({type}/{runtime})

Phase 1 — Inspect:       runtime={type}, CLAUDE.md={present|missing}, project.yaml={present|missing}
Phase 2 — Plugins:       {N} explicit entries written; LSP={plugin_name}
Phase 3 — project.yaml:  {N} fields set (auto/default); {N} fields preserved (sacred)
Phase 4 — Submodules:    {N} added; {N} stale flagged; {N} preserved
Phase 5 — .gitignore:    {N} patterns added; {N} already present
Phase 6 — Hooks:         global guard={ok|updated}; local hooks={ok|updated}; context7={ok|installed|MISSING}
Phase 7 — CLAUDE.md:     core block={already present|inserted|CLAUDE.md created}
Phase 8 — MCP cleanup:   {N} dangling mcp__ entries removed from permissions.allow

Files changed:
  .claude/settings.local.json   {changes summary}
  ~/.claude/settings.json       {changes summary}
  project.yaml                  {created|updated|unchanged}
  .gitignore                    {N patterns added|unchanged}
  CLAUDE.md                     {core block inserted|unchanged}

Warnings (if any):
  {List any ❌ or stale items requiring engineer review}

Next: Restart Claude Code to apply plugin changes.
```

---

## Checklist

```
PHASE 1 — INSPECT
[ ] .claude/ directory present
[ ] settings.local.json read (or {} default)
[ ] Global settings inspected
[ ] project.yaml read
[ ] Runtime detected from file indicators

PHASE 2 — PLUGIN ALLOWLIST
[ ] Decision matrix applied for detected project type
[ ] All 26 known plugins have explicit true/false entry
[ ] includeCoAuthoredBy set to false
[ ] settings.local.json valid JSON after write

PHASE 3 — PROJECT.YAML
[ ] Auto fields derived (name, git.url, type, runtime)
[ ] Default fields set where absent
[ ] Sacred fields left untouched
[ ] project.yaml valid YAML after write

PHASE 4 — SUBMODULES
[ ] .gitmodules parsed
[ ] Missing submodules added to project.yaml
[ ] Stale entries flagged (not removed)

PHASE 5 — .GITIGNORE
[ ] All required patterns verified
[ ] Missing patterns appended

PHASE 6 — HOOKS & GUARDS
[ ] Hook scripts copied to ~/.claude/hooks/
[ ] Global PreToolUse hook ensured (not overwritten)
[ ] Local settings.local.json hooks section updated
[ ] context7 MCP server present
[ ] Guard functional test passed

PHASE 7 — CLAUDE.MD
[ ] Sentinel checked
[ ] Core block inserted if absent

PHASE 8 — MCP PERMISSIONS
[ ] Dangling mcp__ entries identified
[ ] Diff reviewed before applying
[ ] Cleaned file written

PHASE 9 — VALIDATE
[ ] JSON files valid
[ ] YAML files valid
[ ] Report printed
```
