---
version: 1.3.1
description: "JavaScript/Cypress code review for best practices (ESLint, patterns, Page Objects). Usage: /dev-review js"
---

# Code Review for JavaScript/Cypress Best Practices

Review JavaScript/Cypress code for best practices, fix issues, and ensure quality.

> **Note**: Quality checks (ESLint, Prettier) are handled by `/dev-review` orchestrator via `just quality js`. This skill focuses on language-specific patterns.

## Step 1: Use Semantic Tools for Code Analysis

**Use LSP tools** for efficient semantic analysis:

| Tool | Use Case |
|------|----------|
| `documentSymbol` | Get file structure (functions, commands) |
| `goToDefinition` | Navigate to definitions |
| `findReferences` | Find all usages of a symbol |
| `hover` | Get type info without reading entire files |

**Fallback to Grep/Glob** for pattern search across codebase.

**Benefits of LSP:**
- Reduces token usage vs reading full files
- Symbol-level precision for edits
- Cross-file reference tracking

## Step 2: Best Practices Review

Skip items already covered by quality checks (formatting, unused imports).

Focus on these categories:

### 3.1 Constants & Configuration

| Check | Issue | Fix |
|-------|-------|-----|
| Magic numbers | Literals like `5000`, `200` in code | Move to constants or config |
| Magic strings | Selectors like `'.my-class'` repeated | Extract to Page Objects |
| Hardcoded URLs | URLs in test files | Use `Cypress.env()` or config |
| Hardcoded credentials | Usernames/passwords in tests | Move to fixtures or env vars |
| Config defaults | Default values in multiple places | Extract to constants with `DEFAULT_` prefix |

**Example fix:**
```javascript
// Bad
cy.wait(5000);
cy.get('.user-table').should('be.visible');

// Good - use intelligent waits
cy.get('.loading').should('not.exist');
cy.get('.user-table').should('be.visible');

// Good - use Page Objects
import { UserPage } from '../pages/UserPage';
UserPage.userTable.should('be.visible');
```

### 3.2 Cypress Best Practices

| Check | Issue | Fix |
|-------|-------|-----|
| `cy.wait(ms)` | Fixed time waits | Use `cy.intercept()` or assertions |
| Chained `.then()` | Nested callbacks | Use Cypress commands directly |
| `cy.get().click().get()` | Brittle chains | Break into separate commands |
| Missing assertions | Actions without verification | Add explicit assertions |
| Test dependencies | Tests depend on other tests | Make tests independent |

**Example fix:**
```javascript
// Bad - fixed wait
cy.get('.submit').click();
cy.wait(3000);
cy.get('.success').should('exist');

// Good - intercept and wait
cy.intercept('POST', '/api/submit').as('submitRequest');
cy.get('.submit').click();
cy.wait('@submitRequest');
cy.get('.success').should('be.visible');
```

### 3.3 Page Object Pattern

| Check | Issue | Fix |
|-------|-------|-----|
| Selectors in tests | Direct selectors in test files | Move to Page Objects |
| Repeated navigation | Same visit pattern repeated | Create `loadPage()` methods |
| Duplicate selectors | Same selector in multiple files | Centralize in pages.js |

**Example fix:**
```javascript
// Bad - selectors scattered in tests
cy.get('#username').type('admin');
cy.get('#password').type('password');
cy.get('button[type="submit"]').click();

// Good - Page Object pattern
// pages/LoginPage.js
export const LoginPage = {
  selectors: {
    username: '#username',
    password: '#password',
    submit: 'button[type="submit"]'
  },
  login(user, pass) {
    cy.get(this.selectors.username).type(user);
    cy.get(this.selectors.password).type(pass);
    cy.get(this.selectors.submit).click();
  }
};

// In test
LoginPage.login('admin', 'password');
```

### 3.4 Test Structure

| Check | Issue | Fix |
|-------|-------|-----|
| Missing `beforeEach` | Setup duplicated in each test | Use hooks |
| No cleanup | Test data persists | Add `afterEach` cleanup |
| Giant `it` blocks | Single test with many steps | Break into focused tests |
| Unclear test names | Generic test descriptions | Use descriptive names |

**Example fix:**
```javascript
// Bad
it('test user', () => {
  // 50 lines of code doing multiple things
});

// Good
describe('User Management', () => {
  beforeEach(() => {
    cy.login();
    Pages.UserManagement.loadPage();
  });

  it('should display user list', () => { ... });
  it('should create new user', () => { ... });
  it('should delete user', () => { ... });
});
```

### 3.5 Error Handling & Stability

| Check | Issue | Fix |
|-------|-------|-----|
| No retry logic | Flaky elements | Use `cy.get().should()` with retry |
| Uncaught exceptions | Test fails on app errors | Handle with `cy.on()` |
| Race conditions | Elements not ready | Use proper waits/assertions |

**Example fix:**
```javascript
// Handle uncaught exceptions
Cypress.on('uncaught:exception', (err) => {
  if (err.message.includes('Expected error')) {
    return false; // Don't fail test
  }
});

// Use retry-ability
cy.get('.dynamic-element', { timeout: 10000 })
  .should('be.visible')
  .and('contain', 'Expected text');
```

### 3.6 Custom Commands

| Check | Issue | Fix |
|-------|-------|-----|
| Repeated patterns | Same code in multiple tests | Create custom command |
| Complex assertions | Multi-step verification | Create custom assertion |
| API setup | Test data creation | Create `cy.api*` commands |

**Example fix:**
```javascript
// commands.js
Cypress.Commands.add('loginAs', (role) => {
  const users = {
    admin: { username: 'admin', password: 'admin123' },
    user: { username: 'user', password: 'user123' }
  };
  cy.login(users[role].username, users[role].password);
});

// In test
cy.loginAs('admin');
```

### 3.7 Test Data Management

| Check | Issue | Fix |
|-------|-------|-----|
| Inline test data | Data hardcoded in tests | Use fixtures |
| Shared state | Tests modify shared data | Isolate test data |
| Stale data | Tests assume specific state | Reset before each test |

**Example fix:**
```javascript
// fixtures/users.json
{
  "testUser": {
    "username": "testuser",
    "password": "testpass"
  }
}

// In test
cy.fixture('users').then((users) => {
  cy.login(users.testUser.username, users.testUser.password);
});
```

### 3.8 Duplicate Code Detection

| Check | Issue | Fix |
|-------|-------|-----|
| Copy-pasted blocks | 10+ similar lines in multiple places | Extract to shared helper function |
| Similar functions | Functions with same logic, different names | Consolidate into single function |
| Repeated patterns | Same assertion or setup pattern | Extract to custom command |

**How to detect:**
1. Look for functions with similar structure across files
2. Search for identical string literals or patterns
3. Check if same logic exists in multiple tests

**Example fix:**
```javascript
// Bad - duplicate logic in two test files
// userList.test.js
const rows = screen.getAllByRole('row');
expect(rows.length).toBeGreaterThan(0);
fireEvent.click(rows[0]);
expect(screen.getByTestId('detail-panel')).toBeVisible();

// userSearch.test.js
const rows = screen.getAllByRole('row');
expect(rows.length).toBeGreaterThan(0);
fireEvent.click(rows[0]);
expect(screen.getByTestId('detail-panel')).toBeVisible();

// Good - extract to custom command
// commands.js
Cypress.Commands.add('selectFirstTableRow', (tableSelector, detailSelector) => {
  cy.get(tableSelector).find('tr').should('have.length.gt', 0);
  cy.get(tableSelector).find('tr').first().click();
  cy.get(detailSelector).should('be.visible');
});

// In tests
cy.selectFirstTableRow('.user-table', '.user-detail');
```

### 3.9 Security

| Check | Issue | Fix |
|-------|-------|-----|
| Hardcoded secrets | API keys, passwords in code | Use env vars or fixtures |
| Credentials in commits | Passwords in test files | Move to .env.local (gitignored) |
| Sensitive data logged | cy.log() with passwords | Remove or mask sensitive data |

## Step 3: Apply Fixes

1. Make fixes directly to the code
2. Run `just quality all` again to verify
3. Run `just test spec "path/to/changed/spec"` to test changes

## Step 4: Report & Commit

Summarize what was fixed:
- List files modified
- Categories of fixes applied
- Any issues that need manual attention

If changes were made and tests pass, invoke `/dev-commit` skill.

## Review Checklist Summary

```
[ ] Use semantic tools to analyze code structure
[ ] No magic numbers/strings (use constants/Page Objects)
[ ] No cy.wait(ms) - use intercepts and assertions
[ ] Page Object pattern used consistently
[ ] Tests are independent (proper beforeEach/afterEach)
[ ] Custom commands for repeated patterns
[ ] Test data in fixtures, not inline
[ ] No duplicate code (extract shared logic)
[ ] Proper error handling
[ ] No security issues (secrets, credentials)
[ ] All affected tests passing
```
