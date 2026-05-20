---
version: 1.5.0
description: "Python code review for best practices (ruff, mypy, types, constants, SOLID). Usage: /dev-review py"
---

# Code Review for Python Best Practices

Review Python code for best practices, fix issues, and ensure quality.

> **Note**: Quality checks (ruff) are handled by `/dev-review` orchestrator via `just quality all`. This skill focuses on language-specific patterns.

## Step 1: Use Semantic Tools for Code Analysis

**Use LSP tools** for efficient semantic analysis:

| Tool | Use Case |
|------|----------|
| `documentSymbol` | Get file structure (classes, functions, methods) |
| `goToDefinition` | Navigate to definitions |
| `findReferences` | Find all usages of a symbol |
| `hover` | Get type info without reading entire files |

**Fallback to Grep/Glob** for pattern search across codebase.

**Benefits:**

- Reduces token usage vs reading full files
- Symbol-level precision for edits
- Cross-file reference tracking

## Step 2: Best Practices Review

### 2.1 Constants & Enums

| Check | Issue | Fix |
|-------|-------|-----|
| Magic numbers | Literals like `30`, `200` | Move to `constants.py` with `DEFAULT_` prefix |
| Magic strings | Status like `"pending"` | Create Enum (check existing first) |
| Repeated values | Same string 3+ times | Extract to constants |
| Config defaults | Default in multiple places | Use `DEFAULT_`, `MIN_`, `MAX_` prefixes |
| Internal constants | Module-private values | Use `_INTERNAL_NAME` prefix |

**Example fix:**

```python
# Bad
timeout = int(os.getenv("TIMEOUT", "30"))
if limit < 1 or limit > 1000:
    raise ValueError("Invalid")

# Good - in constants.py
DEFAULT_TIMEOUT = 30
MIN_LIMIT = 1
MAX_LIMIT = 1000

# Good - in code
from constants import DEFAULT_TIMEOUT, MIN_LIMIT, MAX_LIMIT
timeout = int(os.getenv("TIMEOUT", str(DEFAULT_TIMEOUT)))
if limit < MIN_LIMIT or limit > MAX_LIMIT:
    raise ValueError(f"Limit must be {MIN_LIMIT}-{MAX_LIMIT}")
```

### 2.2 Type Safety

| Check | Issue | Fix |
|-------|-------|-----|
| `Any` overuse | Using `Any` when specific type possible | Use concrete types or TypeVar |
| Missing type aliases | Complex types repeated | Create type alias |
| Optional ambiguity | `None` handling unclear | Use explicit `T \| None` |
| Type assertions | Excessive `cast()` or `# type: ignore` | Use type guards |

**Example fix:**

```python
# Bad
def process(data: dict[str, Any]) -> Any: ...

# Good
IssueData = dict[str, str | int | list[str]]
def process(data: IssueData) -> ProcessResult: ...
```

### 2.3 SOLID Principles

| Principle | Check | Fix |
|-----------|-------|-----|
| Single Responsibility | Function doing multiple things | Split into focused units |
| Dependency Inversion | Direct instantiation | Accept dependencies as parameters |

**Example fix:**

```python
# Bad - hidden dependency
def upload_file(path: Path):
    client = get_jira_client()
    client.upload(path)

# Good - dependency injection
def upload_file(client: Jira, path: Path):
    client.upload(path)
```

### 2.4 Modularity

| Check | Issue | Fix |
|-------|-------|-----|
| Logic in tools/ | Business logic in CLI | Move to `src/lib/` or `src/stages/` |
| Large files | >300 lines | Split into focused modules |
| Duplicate code | Same logic in multiple places | Extract to shared module |
| Circular imports | A imports B imports A | Use dependency injection |

**Project structure rule:**

- `src/lib/` - Reusable utilities (with tests in `tests/lib/`)
- `src/stages/` - Stage-specific logic (with tests in `tests/stages/`)
- `src/tools/` - CLI wrappers only (thin, no business logic)

### 2.5 Error Handling

| Check | Issue | Fix |
|-------|-------|-----|
| Bare except | `except:` or `except Exception:` | Catch specific exceptions |
| Lost stack trace | `logger.error(str(e))` | Use `logger.exception()` |
| Swallowed errors | Empty except block | Log or re-raise |

**Example fix:**

```python
# Bad
try:
    result = api_call()
except Exception as e:
    logger.error(f"Failed: {e}")
    return None

# Good
try:
    result = api_call()
except RequestError as e:
    logger.exception("API call failed")
    raise
```

### 2.6 Security

| Check | Issue | Fix |
|-------|-------|-----|
| Hardcoded secrets | API keys in code | Use env vars |
| Dynamic code | `eval()`/`exec()` | Use `ast.literal_eval()` or remove |
| Unvalidated input | External input used directly | Validate at boundaries |
| SQL injection | String concatenation | Use parameterized queries |
| Template injection | User input in f-strings | Sanitize or use safe templating |

### 2.7 Duplicate Code Detection

| Check | Issue | Fix |
|-------|-------|-----|
| Copy-pasted blocks | 10+ similar lines | Extract to helper function |
| Similar functions | Same logic, different names | Consolidate |
| Repeated patterns | Same try/catch pattern | Extract to utility |

### 2.8 Testing

| Check | Issue | Fix |
|-------|-------|-----|
| Missing tests | New `src/lib/` without tests | Add tests in `tests/lib/` |
| Missing tests | New `src/stages/` without tests | Add tests in `tests/stages/` |
| Test isolation | Tests sharing state | Use fixtures properly |
| Duplicate tests | Similar test logic | Use `@pytest.mark.parametrize` |

## Step 3: Apply Fixes

1. Make fixes directly to the code
2. Run `just quality all` to verify
3. Run `just test unit` to ensure tests pass

## Step 4: Report & Commit

Summarize what was fixed:

- List files modified
- Categories of fixes applied
- Any issues needing manual attention

If changes were made and tests pass, invoke `/dev-commit`.

## Review Checklist

```text
[ ] Use semantic tools to analyze code
[ ] No magic numbers/strings (use constants/enums)
[ ] Constants use proper prefixes (DEFAULT_, MIN_, MAX_)
[ ] Types are specific (no unnecessary Any)
[ ] SOLID principles followed
[ ] Code properly modularized (lib vs tools vs stages)
[ ] Errors handled with stack traces
[ ] No security issues
[ ] No duplicate code
[ ] Unit tests exist for new code
[ ] All tests passing
```
