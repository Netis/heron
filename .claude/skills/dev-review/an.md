---
version: 1.2.0
description: "Ansible test code review for best practices (ansible-lint, YAML, NPM patterns). Usage: /dev-review an"
---

# Code Review for Ansible Test Best Practices

Review Ansible test files for best practices, fix issues, and ensure quality.

> **Note**: Quality checks (ansible-lint) are handled by `/dev-review` orchestrator via `just quality an`. This skill focuses on NPM-specific patterns.

## Step 1: Use Search Tools for Analysis

**Use Grep/Glob** for pattern analysis:

| Tool | Use Case |
|------|----------|
| `Grep` | Find patterns across test files |
| `Glob` | Locate test files by pattern |
| `Read` | Read specific test content |

**Grep patterns for common issues:**
```bash
# Find non-FQCN modules
grep -r "^\s*- name:" ansible/tests/ | grep -v "ansible.builtin"

# Find missing delegate_to
grep -l "ansible.builtin.assert" ansible/tests/**/*.yml | xargs grep -L "delegate_to"

# Find hardcoded paths
grep -r "/opt/app" ansible/tests/ | grep -v "app_bin"
```

## Step 2: Best Practices Review

Focus on these categories:

### 3.1 NPM Test Structure (Critical)

| Check | Issue | Fix |
|-------|-------|-----|
| Python 2.7 compatibility | Using modules that need Python 3 | Use `ansible.builtin.raw` for commands |
| Assert delegation | Assertions fail on remote | Add `delegate_to: localhost` to asserts |
| Variable usage | Hardcoded `/opt/app/bin/app` | Use `{{ app_bin }}` variable |
| Step/Check comments | Missing step mapping | Add `# === STEP N:` and `# CHECK N:` |
| Header comments | Missing case info | Add `# CASE_ID: description` header |

**Example fix (Python 2.7 workaround):**
```yaml
# Bad - fails on Python 2.7
- name: "Check 1: Verify status"
  ansible.builtin.assert:
    that:
      - "'RUNNING' in status.stdout"

# Good - delegate to localhost (Python 3)
- name: "Check 1: Verify status"
  ansible.builtin.assert:
    that:
      - "'RUNNING' in status.stdout"
    fail_msg: "Process should be RUNNING"
    success_msg: "Process status verified"
  delegate_to: localhost
```

**Example fix (Variable usage):**
```yaml
# Bad - hardcoded path
- name: "Step 1: Run status"
  ansible.builtin.raw: /opt/app/bin/app status

# Good - use variable
- name: "Step 1: Run status"
  ansible.builtin.raw: "{{ app_bin }} status"
  register: status_result
  changed_when: false
```

### 3.2 FQCN (Fully Qualified Collection Names)

| Check | Issue | Fix |
|-------|-------|-----|
| Short names | `shell:`, `raw:`, `assert:` | Use `ansible.builtin.shell`, etc. |
| Mixed styles | Some FQCN, some short | Consistent FQCN throughout |

**Example fix:**
```yaml
# Bad
- name: Run command
  raw: echo "hello"

- name: Check result
  assert:
    that: result.rc == 0

# Good
- name: Run command
  ansible.builtin.raw: echo "hello"

- name: Check result
  ansible.builtin.assert:
    that: result.rc == 0
  delegate_to: localhost
```

### 3.3 Task Naming & Documentation

| Check | Issue | Fix |
|-------|-------|-----|
| Missing names | Tasks without `name:` | Add descriptive names |
| Generic names | "Run command", "Check result" | Use specific descriptions |
| Missing header | No case ID comment | Add header block |
| Missing step comments | No `# === STEP N:` | Map to case JSON steps |

**Example fix (Header block):**
```yaml
# CMD006: app console status
# Priority: P0
# Module: CLI Commands
# Maps to: cases/APP/functional/cli/CMD006.json
---
- name: "CMD006: app console status"
  block:
    # === STEP 1: Execute status all in console ===
    - name: "Step 1: Execute 'status all' in app console"
      ansible.builtin.raw: |
        {{ app_bin }} console << 'EOF'
        status all
        exit
        EOF
      register: status_all
      changed_when: false

    # CHECK 1: Verify status all shows process info
    - name: "Check 1: Verify status all shows process status"
      ansible.builtin.assert:
        that:
          - status_all.rc == 0
          - "'RUNNING' in status_all.stdout or 'STOPPED' in status_all.stdout"
        fail_msg: "status all should show process status"
        success_msg: "status all shows process status correctly"
      delegate_to: localhost
```

### 3.4 Register & Changed When

| Check | Issue | Fix |
|-------|-------|-----|
| Missing register | Command result not captured | Add `register: result_name` |
| False changed | Read-only commands show changed | Add `changed_when: false` |
| Unused register | Registered but never used | Remove or use in assertion |

**Example fix:**
```yaml
# Bad
- name: "Step 1: Get status"
  ansible.builtin.raw: "{{ app_bin }} status"

- name: "Check 1: Verify"
  ansible.builtin.assert:
    that: true  # What are we checking?

# Good
- name: "Step 1: Get status"
  ansible.builtin.raw: "{{ app_bin }} status"
  register: status_result
  changed_when: false

- name: "Check 1: Verify status output"
  ansible.builtin.assert:
    that:
      - status_result.rc == 0
      - "'RUNNING' in status_result.stdout"
  delegate_to: localhost
```

### 3.5 Assertion Quality

| Check | Issue | Fix |
|-------|-------|-----|
| Weak assertions | Only checking `rc == 0` | Add content verification |
| Missing messages | No fail_msg/success_msg | Add descriptive messages |
| Overly strict | Exact string match | Use `in` or regex for flexibility |

**Example fix:**
```yaml
# Bad - weak assertion
- name: "Check 1: Verify"
  ansible.builtin.assert:
    that:
      - result.rc == 0
  delegate_to: localhost

# Good - comprehensive assertion
- name: "Check 1: Verify version output format"
  ansible.builtin.assert:
    that:
      - result.rc == 0
      - "'appVersion' in result.stdout"
      - "'buildVersion' in result.stdout"
    fail_msg: "verinfo should show appVersion and buildVersion"
    success_msg: "Version info displayed correctly"
  delegate_to: localhost
```

### 3.6 YAML Best Practices

| Check | Issue | Fix |
|-------|-------|-----|
| Indentation | Inconsistent (2 vs 4 spaces) | Use 2-space indentation |
| Boolean | Using `yes`/`no`/`on`/`off` | Use `true`/`false` |
| Quoting | Unquoted special chars | Quote strings with `:`, `{`, `}` |
| Long lines | Lines > 120 chars | Break into multiple lines |
| Multiline | Complex commands on one line | Use `|` for readability |

**Example fix:**
```yaml
# Bad
- name: Run command
  ansible.builtin.raw: {{ app_bin }} console << 'EOF'
status all
exit
EOF
  register: result

# Good
- name: Run command
  ansible.builtin.raw: |
    {{ app_bin }} console << 'EOF'
    status all
    exit
    EOF
  register: result
  changed_when: false
```

### 3.7 Test Independence

| Check | Issue | Fix |
|-------|-------|-----|
| Shared state | Test assumes state from other test | Make self-contained |
| No cleanup | Creates data but doesn't clean | Add cleanup tasks or use idempotent |
| Order dependency | Must run after another test | Remove dependency |

**Example fix:**
```yaml
# Bad - depends on previous test creating data
- name: "Step 1: Use existing config"
  ansible.builtin.raw: "{{ app_bin }} config show"

# Good - self-contained, verifies own preconditions
- name: "Step 1: Verify config exists"
  ansible.builtin.raw: "test -f {{ app_home }}/etc/config.toml"
  register: config_check
  failed_when: config_check.rc != 0
  changed_when: false
```

### 3.8 Security

| Check | Issue | Fix |
|-------|-------|-----|
| Hardcoded secrets | Passwords in test files | Use ansible-vault or env vars |
| Sensitive output | Commands expose credentials | Add `no_log: true` |
| Overly permissive | Running as root unnecessarily | Minimize privileges |

**Example fix:**
```yaml
# Bad
- name: Login with password
  ansible.builtin.raw: "echo 'secretpassword' | {{ app_bin }} login"

# Good
- name: Login with password
  ansible.builtin.raw: "echo '{{ app_password }}' | {{ app_bin }} login"
  no_log: true
```

### 3.9 Organization

| Check | Issue | Fix |
|-------|-------|-----|
| Large files | Test > 200 lines | Split into smaller focused tests |
| Repeated code | Same pattern in multiple files | Extract to shared tasks |
| Naming | Inconsistent file names | Use CASE_ID.yml pattern |

**File naming convention:**
```
ansible/tests/
├── cmd/
│   ├── CMD001.yml  # Single test per file
│   ├── CMD002.yml
│   └── ...
└── config/
    ├── CFC001.yml
    ├── CFC002.yml
    └── ...
```

### 3.10 Module Usage

| Check | Issue | Fix |
|-------|-------|-----|
| `command:` for package install | Use proper module | Use `apt:`, `yum:`, `dnf:` |
| `command: systemctl` | Use proper module | Use `systemd:` module |
| `command: ufw` | Use proper module | Use `community.general.ufw` |
| `command: sysctl` | Use proper module | Use `ansible.posix.sysctl` |
| `template:` without `.j2` extension | Confusing file types | Use `.j2` extension for templates |

### 3.11 Handler Best Practices

| Check | Issue | Fix |
|-------|-------|-----|
| `notify:` without matching handler | Silent failure | Verify handler name matches |
| Service restart without handler | Not triggered on change | Use `notify:` + handler |
| Handler order dependency | Unpredictable execution | Use `meta: flush_handlers` |
| Missing `listen:` for related handlers | Multiple notifies needed | Use `listen:` topic |

### 3.12 Error Handling

| Check | Issue | Fix |
|-------|-------|-----|
| `ignore_errors: true` | Silent failures | Use `failed_when:` or `rescue:` |
| Missing `retries:` on network tasks | Flaky failures | Add `retries:` with `delay:` |
| No `block:`/`rescue:` for critical sections | Unhandled failures | Wrap in block/rescue |
| Missing health checks after service start | Unknown state | Add `uri:` or `wait_for:` check |

### 3.13 Idempotency

| Check | Issue | Fix |
|-------|-------|-----|
| `command:` without `creates:` | Non-idempotent task | Add `creates:` or use proper module |
| `shell:` without `creates:` | Non-idempotent task | Use `command:` with `creates:` or proper module |
| Missing `changed_when` | Misleading change reporting | Add `changed_when: false` for read-only commands |

### 3.14 Duplicate Code Detection

| Check | Issue | Fix |
|-------|-------|-----|
| Repeated task patterns | Copy-pasted blocks | Extract to role or include |
| Similar handlers across files | Duplication | Consolidate in shared handlers |
| Repeated `when:` conditions | Complex conditionals | Use group membership or tags |

## Step 3: Apply Fixes

1. Make fixes directly to the test files
2. Run specific test: `just ansible test CMD006`

## Step 4: Report & Commit

Summarize what was fixed:
- List files modified
- Categories of fixes applied
- Any issues that need manual attention

If changes were made and tests pass, invoke `/dev-commit` skill.

## Review Checklist Summary

```
[ ] Python 2.7 compatibility (raw + delegate_to)
[ ] Use {{ app_bin }} variable (no hardcoded paths)
[ ] FQCN for all modules
[ ] All tasks have descriptive names
[ ] Step/Check comments match case JSON
[ ] Header block with case info
[ ] register + changed_when for commands
[ ] Comprehensive assertions with messages
[ ] delegate_to: localhost for all asserts
[ ] Consistent YAML formatting
[ ] Tests are independent
[ ] No security issues
[ ] All affected tests passing
```

## Related Documentation

| Document | Content |
|----------|---------|
| [context/npm/04-backend.md](../../context/npm/04-backend.md) | NPM CLI commands and config |
| [docs/60_Ansible测试框架.md](../../docs/60_Ansible测试框架.md) | Ansible framework details |
| [.claude/skills/test-pdca/skill.md](../test-pdca/skill.md) | Unified test PDCA workflow |
