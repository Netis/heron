---
version: 1.2.0
description: "TypeScript code review for best practices (ESLint, types, constants, SOLID). Usage: /dev-review ts"
---

# Code Review for TypeScript Best Practices

Review code for best practices, fix issues, and ensure quality.

> **Note**: Quality checks (ESLint, Prettier, tsc) are handled by `/dev-review` orchestrator via `just quality all`. This skill focuses on language-specific patterns.

## Step 1: Use Semantic Tools for Code Analysis

**Prefer Serena MCP tools** for efficient semantic analysis:

| Tool | Use Case |
|------|----------|
| `get_symbols_overview` | Get file structure (classes, functions, methods) |
| `find_symbol` | Find symbols by name pattern with `include_body=true` |
| `find_referencing_symbols` | Find all usages of a symbol |
| `search_for_pattern` | Search for patterns across codebase |

**Fallback to LSP tools** if Serena is not available:
- `hover`: Get type info without reading entire files
- `goToDefinition`: Navigate to definitions
- `findReferences`: Find all usages
- `documentSymbol`: Get file structure

**Benefits of semantic tools:**
- Reduces token usage vs reading full files
- Symbol-level precision for edits
- Cross-file reference tracking

## Step 2: Best Practices Review

Skip items already covered by quality checks (formatting, unused imports, basic type errors).

Focus on these categories:

### 2.1 Constants & Enums

| Check | Issue | Fix |
|-------|-------|-----|
| Magic numbers | Literals like `30`, `200` in code | Move to constants file or use `as const` |
| Magic strings | Status strings like `"pending"`, `"active"` | Create enum or const object |
| Repeated values | Same string/path used 3+ times | Extract to constants |
| Hardcoded config | URLs, paths, field names | Move to constants or env vars |
| Config defaults | Default values in multiple places | Extract to constants with `DEFAULT_`, `MIN_`, `MAX_` prefixes |
| Internal constants | Module-private values | Use `_INTERNAL_NAME` prefix (not exported) |

**Example fix (basic):**
```typescript
// Bad
const timeout = parseInt(process.env.TIMEOUT ?? '30', 10)

// Good - in constants.ts
export const DEFAULT_TIMEOUT = 30

// Good - in code
import { DEFAULT_TIMEOUT } from '@/lib/constants'
const timeout = parseInt(process.env.TIMEOUT ?? String(DEFAULT_TIMEOUT), 10)
```

**Example fix (config with bounds):**
```typescript
// Bad - magic numbers in schema
const schema = z.object({
  cacheHours: z.number().min(1).max(168).default(24)
})

// Good - in constants.ts
export const DEFAULT_CACHE_HOURS = 24
export const MIN_CACHE_HOURS = 1
export const MAX_CACHE_HOURS = 168

// Good - in schema
import { DEFAULT_CACHE_HOURS, MIN_CACHE_HOURS, MAX_CACHE_HOURS } from '@/lib/constants'
const schema = z.object({
  cacheHours: z.number()
    .min(MIN_CACHE_HOURS)
    .max(MAX_CACHE_HOURS)
    .default(DEFAULT_CACHE_HOURS)
})
```

**Example fix (internal constants):**
```typescript
// For module-private constants (not exported)
const _RETRY_DELAY = 5  // seconds, internal use only
const _MAX_BUFFER_SIZE = 1024
```

### 2.2 Type Safety (Beyond tsc basics)

| Check | Issue | Fix |
|-------|-------|-----|
| `any` overuse | Using `any` when specific type possible | Use concrete types or generics |
| Missing type aliases | Complex types repeated | Create type alias in shared/ |
| Optional ambiguity | `undefined` handling unclear | Use explicit `T | undefined` or optional chaining |
| Type assertions | Excessive `as Type` casts | Use type guards or proper typing |

**Example fix:**
```typescript
// Bad
function process(data: any): any { ... }

// Good
type PacketData = {
  frameNumber: number
  protocol: string
  info: string
}
function process(data: PacketData): ProcessResult { ... }
```

### 2.3 SOLID Principles

| Principle | Check | Fix |
|-----------|-------|-----|
| Single Responsibility | Function/class doing multiple things | Split into focused units |
| Dependency Inversion | Direct instantiation of dependencies | Accept dependencies as parameters |

**Example fix:**
```typescript
// Bad - creates its own dependency
async function uploadFile(path: string) {
  const client = getApiClient()  // Hidden dependency
  await client.upload(path)
}

// Good - dependency injection
async function uploadFile(client: ApiClient, path: string) {
  await client.upload(path)
}
```

### 2.4 Error Handling

| Check | Issue | Fix |
|-------|-------|-----|
| Bare catch | `catch (e)` without type check | Check error type or use type guard |
| Lost stack trace | `console.error(e.message)` | Log full error object |
| Swallowed errors | Empty catch block | Log or re-throw |
| Unhandled promises | Missing `.catch()` or try/catch | Add proper error handling |

**Example fix:**
```typescript
// Bad
try {
  result = await apiCall()
} catch (e) {
  console.error(`Failed: ${e}`)
  return null
}

// Good
try {
  result = await apiCall()
} catch (e) {
  console.error('API call failed:', e)
  throw e
}
```

### 2.5 Security

| Check | Issue | Fix |
|-------|-------|-----|
| Hardcoded secrets | API keys, passwords in code | Use env vars |
| Dynamic code exec | Functions that run strings as code | Remove or use safer alternatives |
| Unvalidated input | External input used directly | Validate with Zod or similar |
| Injection attacks | String concatenation for queries | Use parameterized queries |
| XSS vulnerabilities | Rendering unsanitized HTML | Sanitize with DOMPurify or avoid |

### 2.6 Modularity

| Check | Issue | Fix |
|-------|-------|-----|
| Logic in routes | Business logic in route handlers | Move to services/ |
| Large files | >300 lines | Split into focused modules |
| Duplicate code | Same logic in multiple places | Extract to shared module |
| Cross-package imports | client importing from server | Use shared/ package for common types |

**Project structure:**
```
packages/
├── client/src/
│   ├── features/     # Feature modules
│   ├── components/   # Shared UI
│   ├── hooks/        # React hooks
│   └── lib/          # Utilities
├── server/src/
│   ├── routes/       # API routes
│   └── services/     # Business logic
└── shared/src/
    └── types/        # Shared types
```

### 2.7 Testing

| Check | Issue | Fix |
|-------|-------|-----|
| Missing tests | New service code without tests | Add tests |
| Test isolation | Tests sharing state | Use proper setup/teardown |
| Duplicate tests | Similar test logic repeated | Use test utilities |

### 2.8 Duplicate Code Detection

| Check | Issue | Fix |
|-------|-------|-----|
| Copy-pasted blocks | 10+ similar lines in multiple places | Extract to shared helper function |
| Similar functions | Functions with same logic, different names | Consolidate into single function |
| Repeated patterns | Same try/catch or loop pattern | Extract to utility function |

**How to detect:**
1. Look for functions with similar structure across files
2. Search for identical string literals or patterns
3. Check if same logic exists in multiple components

**Example fix:**
```typescript
// Bad - duplicate logic in two components
function PacketCard({ packet }) {
  // ... 20 lines of formatting logic ...
}

function PacketListItem({ packet }) {
  // ... same 20 lines of formatting logic ...
}

// Good - extract shared logic
function formatPacketDisplay(packet: Packet): DisplayData {
  // ... 20 lines in one place ...
}

function PacketCard({ packet }) {
  const display = formatPacketDisplay(packet)
  // ... render ...
}

function PacketListItem({ packet }) {
  const display = formatPacketDisplay(packet)
  // ... render ...
}
```

## Step 3: Apply Fixes

1. Make fixes directly to the code
2. Run `just quality all` again to verify
3. Run `bun test` to ensure tests pass (if tests exist)

## Step 4: Report & Commit

Summarize what was fixed:
- List files modified
- Categories of fixes applied
- Any issues that need manual attention

If changes were made and checks pass, invoke `/dev-commit` skill.

## Review Checklist Summary

```
[ ] Use semantic tools to analyze code structure
[ ] No magic numbers/strings (use constants/enums)
[ ] Config defaults/bounds use constants (DEFAULT_, MIN_, MAX_)
[ ] Types are specific (no unnecessary any)
[ ] SOLID principles followed
[ ] Errors handled properly with stack traces
[ ] No security issues (secrets, dynamic code exec, unvalidated input)
[ ] Code properly modularized (shared types, services)
[ ] No duplicate code (extract shared logic)
[ ] Tests exist for new code (if applicable)
[ ] All checks passing
```
